// sentiric-telecom-client-sdk/src/media/hardware.rs

use super::adapter::MediaAdapter;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{HeapRb, SharedRb, Producer, Consumer};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tracing::{error, info};

// Karmaşık RingBuf tiplerini gizliyoruz
type RbProd = Producer<f32, Arc<SharedRb<f32, Vec<std::mem::MaybeUninit<f32>>>>>;
type RbCons = Consumer<f32, Arc<SharedRb<f32, Vec<std::mem::MaybeUninit<f32>>>>>;

pub struct HardwareAdapter {
    mic_cons: Arc<Mutex<RbCons>>,
    spk_prod: Arc<Mutex<RbProd>>,
    mic_gain: Arc<AtomicU32>,
    spk_gain: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    is_healthy: Arc<AtomicBool>,
    hw_sample_rate_in: usize,
    hw_sample_rate_out: usize,
    // Stream'leri yaşatan bekçi kanalı. Send + Sync garantilidir.
    _keep_alive_tx: std::sync::mpsc::Sender<()>, 
}

impl HardwareAdapter {
    pub fn new() -> anyhow::Result<Self> {
        let rb_in = HeapRb::<f32>::new(48000);
        let (mic_prod, mic_cons) = rb_in.split();
        let shared_mic_prod = Arc::new(Mutex::new(mic_prod));
        let shared_mic_cons = Arc::new(Mutex::new(mic_cons));

        let rb_out = HeapRb::<f32>::new(48000);
        let (spk_prod, spk_cons) = rb_out.split();
        let shared_spk_prod = Arc::new(Mutex::new(spk_prod));
        let shared_spk_cons = Arc::new(Mutex::new(spk_cons));

        let is_muted = Arc::new(AtomicBool::new(false));
        let is_healthy = Arc::new(AtomicBool::new(true));
        let mic_gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let spk_gain = Arc::new(AtomicU32::new(1.2f32.to_bits()));

        // Sample rate'leri thread'den dışarı taşıyabilmek için atomikler kullanıyoruz
        let hw_sample_rate_in = Arc::new(AtomicU32::new(16000));
        let hw_sample_rate_out = Arc::new(AtomicU32::new(16000));

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
        let (keep_alive_tx, keep_alive_rx) = std::sync::mpsc::channel::<()>();

        // Closure (Thread) içine taşınacak referans kopyaları
        let t_prod = shared_mic_prod.clone();
        let t_cons = shared_spk_cons.clone();
        let t_l_mic = mic_gain.clone();
        let t_l_spk = spk_gain.clone();
        let t_l_muted = is_muted.clone();
        let t_healthy_in = is_healthy.clone();
        let t_healthy_out = is_healthy.clone();
        let t_sr_in = hw_sample_rate_in.clone();
        let t_sr_out = hw_sample_rate_out.clone();

        // [MİMARİ: THE TRUE KEEPER THREAD]
        // cpal::Stream nesneleri (!Send) asla bu thread'in dışına çıkmaz.
        std::thread::Builder::new()
            .name("cpal-keeper".into())
            .spawn(move || {
                let host = cpal::default_host();
                
                let input_device = match host.default_input_device() {
                    Some(d) => d,
                    None => { let _ = ready_tx.send(Err(anyhow::anyhow!("No input device"))); return; }
                };
                
                let output_device = match host.default_output_device() {
                    Some(d) => d,
                    None => { let _ = ready_tx.send(Err(anyhow::anyhow!("No output device"))); return; }
                };

                let mut input_config: Option<cpal::StreamConfig> = None;
                if let Ok(mut configs) = input_device.supported_input_configs() {
                    if let Some(mono) = configs.find(|c| c.channels() == 1) {
                        input_config = Some(mono.with_max_sample_rate().into());
                    }
                }
                let input_config = input_config.unwrap_or_else(|| cpal::StreamConfig {
                    channels: 1, sample_rate: cpal::SampleRate(16000), buffer_size: cpal::BufferSize::Default,
                });

                let output_config = output_device.default_output_config()
                    .map(|c| c.into())
                    .unwrap_or_else(|_| cpal::StreamConfig {
                        channels: 1, sample_rate: cpal::SampleRate(16000), buffer_size: cpal::BufferSize::Default,
                    });

                t_sr_in.store(input_config.sample_rate.0, Ordering::Relaxed);
                t_sr_out.store(output_config.sample_rate.0, Ordering::Relaxed);

                let in_channels = input_config.channels as usize;
                let out_channels = output_config.channels as usize;
                
                let mut last_sample_out = 0.0f32;

                // Giriş (Mic) Akışı
                let input_stream = match input_device.build_input_stream(
                    &input_config,
                    move |data: &[f32], _: &_| {
                        if t_l_muted.load(Ordering::Relaxed) { return; } 
                        if let Ok(mut producer) = t_prod.try_lock() {
                            let gain = f32::from_bits(t_l_mic.load(Ordering::Relaxed));
                            if in_channels == 1 {
                                for &s in data { let _ = producer.push(s * gain); }
                            } else {
                                for frame in data.chunks(in_channels) {
                                    let avg = frame.iter().sum::<f32>() / in_channels as f32;
                                    let _ = producer.push(avg * gain);
                                }
                            }
                        }
                    },
                    move |e| { error!("Mic Error: {}", e); t_healthy_in.store(false, Ordering::SeqCst); },
                    None
                ) {
                    Ok(s) => s,
                    Err(e) => { let _ = ready_tx.send(Err(anyhow::anyhow!("Mic build fail: {}", e))); return; }
                };

                // Çıkış (Hoparlör) Akışı
                let output_stream = match output_device.build_output_stream(
                    &output_config,
                    move |data: &mut [f32], _: &_| {
                        if let Ok(mut consumer) = t_cons.try_lock() {
                            let gain = f32::from_bits(t_l_spk.load(Ordering::Relaxed));
                            for frame in data.chunks_mut(out_channels) {
                                let sample = consumer.pop().unwrap_or_else(|| {
                                    last_sample_out *= 0.8;
                                    last_sample_out
                                });
                                last_sample_out = sample;
                                let final_sample = (sample * gain).clamp(-1.0, 1.0);
                                for s in frame.iter_mut() { *s = final_sample; }
                            }
                        } else {
                            for s in data.iter_mut() { *s = 0.0; }
                        }
                    },
                    move |e| { error!("Spk Error: {}", e); t_healthy_out.store(false, Ordering::SeqCst); },
                    None
                ) {
                    Ok(s) => s,
                    Err(e) => { let _ = ready_tx.send(Err(anyhow::anyhow!("Spk build fail: {}", e))); return; }
                };

                if let Err(e) = input_stream.play() { let _ = ready_tx.send(Err(anyhow::anyhow!("Mic play fail: {}", e))); return; }
                if let Err(e) = output_stream.play() { let _ = ready_tx.send(Err(anyhow::anyhow!("Spk play fail: {}", e))); return; }

                // Hazır olduğunu ana thread'e bildir
                let _ = ready_tx.send(Ok(()));
                
                // Bu thread burada uyuyarak stream'leri bellekte yaşatır.
                let _ = keep_alive_rx.recv();
                info!("Hardware Adapter dropped. Audio streams closed safely.");
            })?;

        // Thread'in sonucu dönmesini bekle
        ready_rx.recv().map_err(|e| anyhow::anyhow!("Thread communication error: {}", e))??;

        Ok(Self {
            mic_cons: shared_mic_cons,
            spk_prod: shared_spk_prod,
            mic_gain,
            spk_gain,
            is_muted,
            is_healthy,
            hw_sample_rate_in: hw_sample_rate_in.load(Ordering::Relaxed) as usize,
            hw_sample_rate_out: hw_sample_rate_out.load(Ordering::Relaxed) as usize,
            _keep_alive_tx: keep_alive_tx,
        })
    }
}

impl MediaAdapter for HardwareAdapter {
    fn read_mic(&self, target_8k_samples: usize) -> Vec<i16> {
        let ratio = self.hw_sample_rate_in as f32 / 8000.0;
        let hw_frame_size = (target_8k_samples as f32 * ratio).ceil() as usize;

        let mut cons = self.mic_cons.lock().unwrap();
        
        if self.is_muted.load(Ordering::Relaxed) {
            for _ in 0..cons.len() { let _ = cons.pop(); }
            return vec![0; target_8k_samples]; // Saf sessizlik
        }

        if cons.len() < hw_frame_size {
            return vec![]; 
        }

        let mut mic_data = Vec::with_capacity(hw_frame_size);
        for _ in 0..hw_frame_size {
            let s = cons.pop().unwrap_or(0.0);
            mic_data.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
        }

        sentiric_rtp_core::simple_resample(&mic_data, self.hw_sample_rate_in, 8000)
    }

    fn write_spk(&self, samples_8k: &[i16]) {
        let resampled = sentiric_rtp_core::simple_resample(samples_8k, 8000, self.hw_sample_rate_out);
        let mut prod = self.spk_prod.lock().unwrap();
        for s in resampled {
            let _ = prod.push(s as f32 / 32768.0);
        }
    }

    fn is_healthy(&self) -> bool {
        self.is_healthy.load(Ordering::SeqCst)
    }

    fn set_mute(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    fn update_gains(&self, mic: f32, spk: f32) {
        self.mic_gain.store(mic.to_bits(), Ordering::Relaxed);
        self.spk_gain.store(spk.to_bits(), Ordering::Relaxed);
    }
}