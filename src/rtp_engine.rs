// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, AtomicU8, Ordering};
use ringbuf::HeapRb;
use tracing::error; 
use sentiric_rtp_core::{CodecFactory, CodecType, Pacer, RtpHeader, RtpPacket, simple_resample};
use std::panic;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;
use crate::UacEvent;

pub struct RtpEngine {
    socket: Arc<UdpSocket>,
    is_running: Arc<AtomicBool>,
    pub rx_count: Arc<AtomicU64>,
    pub tx_count: Arc<AtomicU64>,
    headless_mode: bool,
    event_tx: mpsc::Sender<UacEvent>,
    mic_gain: Arc<AtomicU32>,
    speaker_gain: Arc<AtomicU32>,
    dtmf_queue: Arc<AtomicU8>, 
}

fn dtmf_char_to_event(c: char) -> u8 {
    match c {
        '0'..='9' => (c as u8) - b'0',
        '*' => 10,
        '#' => 11,
        'A'..='D' => 12 + ((c as u8) - b'A'),
        _ => 255,
    }
}

impl RtpEngine {
    pub fn new(socket: Arc<UdpSocket>, headless: bool, event_tx: mpsc::Sender<UacEvent>) -> Self {
        Self {
            socket,
            is_running: Arc::new(AtomicBool::new(false)),
            rx_count: Arc::new(AtomicU64::new(0)),
            tx_count: Arc::new(AtomicU64::new(0)),
            headless_mode: headless,
            event_tx,
            mic_gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            speaker_gain: Arc::new(AtomicU32::new(1.2f32.to_bits())),
            dtmf_queue: Arc::new(AtomicU8::new(255)),
        }
    }

    pub fn update_gains(&self, mic: f32, spk: f32) {
        self.mic_gain.store(mic.to_bits(), Ordering::Relaxed);
        self.speaker_gain.store(spk.to_bits(), Ordering::Relaxed);
    }

    pub fn trigger_dtmf(&self, key: char) {
        let event_id = dtmf_char_to_event(key);
        if event_id != 255 {
            self.dtmf_queue.store(event_id, Ordering::Relaxed);
        }
    }

    pub fn start(&self, target: SocketAddr) {
        if self.is_running.swap(true, Ordering::SeqCst) { return; }
        
        let is_running = self.is_running.clone();
        let socket = self.socket.clone();
        let rx_cnt = self.rx_count.clone();
        let tx_cnt = self.tx_count.clone();
        let headless = self.headless_mode;
        let ui_tx = self.event_tx.clone();
        
        let live_mic = self.mic_gain.clone();
        let live_spk = self.speaker_gain.clone();
        let dtmf_queue = self.dtmf_queue.clone();

        std::thread::Builder::new()
            .name("rtp-worker".to_string())
            .spawn(move || {
                let is_running_inner = is_running.clone();
                let ui_tx_inner = ui_tx.clone();
                
                let result = panic::catch_unwind(move || {
                    if headless {
                        let _ = ui_tx_inner.blocking_send(UacEvent::Log("👻 Booting Virtual DSP".into()));
                    } else {
                        let _ = ui_tx_inner.blocking_send(UacEvent::Log("🎤 Booting Hardware Audio (Anti-Crackle Active)...".into()));
                        if let Err(e) = run_hardware_loop(is_running_inner.clone(), socket.clone(), target, rx_cnt.clone(), tx_cnt.clone(), live_mic, live_spk, dtmf_queue) {
                            let _ = ui_tx_inner.blocking_send(UacEvent::Log(format!("⚠️ Hardware Failed: {}", e)));
                        }
                    }
                    is_running_inner.store(false, Ordering::SeqCst);
                });
                
                if let Err(err) = result {
                    let msg = if let Some(s) = err.downcast_ref::<&str>() { s.to_string() } else { "Unknown panic".to_string() };
                    let _ = ui_tx.blocking_send(UacEvent::Error(format!("☠️ RTP Panicked: {}", msg)));
                    is_running.store(false, Ordering::SeqCst);
                }
            }).unwrap();
    }

    pub fn stop(&self) {
        self.is_running.store(false, Ordering::SeqCst);
    }
}

fn run_hardware_loop(
    is_running: Arc<AtomicBool>, socket: Arc<UdpSocket>, target: SocketAddr,
    rx_cnt: Arc<AtomicU64>, tx_cnt: Arc<AtomicU64>,
    live_mic_gain: Arc<AtomicU32>, live_speaker_gain: Arc<AtomicU32>,
    dtmf_queue: Arc<AtomicU8>
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    
    let rb_in = HeapRb::<f32>::new(48000); 
    let (mic_prod, mut mic_cons) = rb_in.split();
    let shared_mic_prod = Arc::new(Mutex::new(mic_prod));

    let rb_out = HeapRb::<f32>::new(48000);
    let (mut spk_prod, spk_cons) = rb_out.split();
    let shared_spk_cons = Arc::new(Mutex::new(spk_cons));

    let codec_type = CodecType::PCMU;
    let payload_type = 0; 
    let dtmf_payload_type = 101; 

    let mut encoder = CodecFactory::create_encoder(codec_type);
    let mut decoder = CodecFactory::create_decoder(codec_type);
    let mut pacer = Pacer::new(20);
    
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = rand::random();
    
    let target_8k_samples = 160; 
    let mut recv_buf = [0u8; 1500];

    let input_device = host.default_input_device().ok_or(anyhow::anyhow!("No input device"))?;
    let output_device = host.default_output_device().ok_or(anyhow::anyhow!("No output device"))?;

    let mut input_config: Option<cpal::StreamConfig> = None;
    if let Ok(mut configs) = input_device.supported_input_configs() {
        if let Some(mono_config) = configs.find(|c| c.channels() == 1) {
            input_config = Some(mono_config.with_max_sample_rate().into());
        }
    }
    
    let input_stream_config = input_config.unwrap_or_else(|| cpal::StreamConfig {
        channels: 1, sample_rate: cpal::SampleRate(16000), buffer_size: cpal::BufferSize::Default,
    });

    let output_config = output_device.default_output_config()
        .map(|c| c.into())
        .unwrap_or_else(|_| cpal::StreamConfig {
            channels: 1, sample_rate: cpal::SampleRate(16000), buffer_size: cpal::BufferSize::Default,
        });

    let hw_sample_rate_in = input_stream_config.sample_rate.0 as usize;
    let hw_sample_rate_out = output_config.sample_rate.0 as usize;
    let in_channels = input_stream_config.channels as usize;
    let out_channels = output_config.channels as usize;
    
    let hw_frame_size = (hw_sample_rate_in * 20) / 1000;
    let stream_healthy = Arc::new(AtomicBool::new(true));
    
    // --- INPUT STREAM (MIC) ---
    let err_fn_in = { let s = stream_healthy.clone(); move |e| { error!("Mic Error: {}", e); s.store(false, Ordering::SeqCst); } };
    let l_mic = live_mic_gain.clone();
    let mic_prod_clone = shared_mic_prod.clone(); 
    
    let input_stream = input_device.build_input_stream(
        &input_stream_config,
        move |data: &[f32], _: &_| {
            if let Ok(mut producer) = mic_prod_clone.try_lock() {
                let gain = f32::from_bits(l_mic.load(Ordering::Relaxed));
                if in_channels == 1 {
                    for &s in data { let _ = producer.push(s * gain); }
                } else {
                    for frame in data.chunks(in_channels) {
                        let avg = frame.iter().sum::<f32>() / in_channels as f32;
                        let _ = producer.push(avg * gain);
                    }
                }
            }
        }, err_fn_in, None
    )?;

    // --- OUTPUT STREAM (SPEAKER - ANTI-CRACKLE) ---
    let err_fn_out = { let s = stream_healthy.clone(); move |e| { error!("Spk Error: {}", e); s.store(false, Ordering::SeqCst); } };
    let l_spk = live_speaker_gain.clone();
    let spk_cons_clone = shared_spk_cons.clone(); 
    
    // [ANTI-CRACKLE]: State for Jitter Fade
    let mut last_spk_sample: f32 = 0.0;

    let output_stream = output_device.build_output_stream(
        &output_config,
        move |data: &mut [f32], _: &_| {
            if let Ok(mut consumer) = spk_cons_clone.try_lock() {
                let gain = f32::from_bits(l_spk.load(Ordering::Relaxed));
                for frame in data.chunks_mut(out_channels) {
                    
                    // Buffer boşsa aniden 0'a düşme, yumuşakça sönümle (Decay).
                    let mut sample = consumer.pop().unwrap_or_else(|| {
                        last_spk_sample *= 0.85; 
                        last_spk_sample
                    });
                    
                    sample *= gain;
                    
                    // Soft-Clipping
                    sample = sample / (1.0 + sample.abs() * 0.3); 
                    last_spk_sample = sample;
                    
                    for s in frame.iter_mut() { *s = sample.clamp(-1.0, 1.0); }
                }
            } else {
                 for s in data.iter_mut() { *s = 0.0; }
            }
        }, err_fn_out, None
    )?;

    input_stream.play()?;
    output_stream.play()?;

    while mic_cons.pop().is_some() {} 
    pacer.reset(); 

    while is_running.load(Ordering::SeqCst) && stream_healthy.load(Ordering::SeqCst) {
        pacer.wait();

        if mic_cons.len() > hw_sample_rate_in / 2 {
            while mic_cons.pop().is_some() {}
        }

        // [DTMF INJECTION]
        let dtmf_event = dtmf_queue.swap(255, Ordering::Relaxed);
        if dtmf_event != 255 {
            // [KRİTİK DÜZELTME]: Türü u16 olarak belirttik
            let mut dtmf_duration: u16 = 160;
            
            for i in 0..5 {
                let end_bit = if i == 4 { 0x80 } else { 0x00 };
                let volume = 10; 
                let mut payload = vec![dtmf_event, end_bit | volume];
                
                // to_be_bytes() artık u16 üzerinden çalışıp 2 byte döndürecek
                payload.extend_from_slice(&dtmf_duration.to_be_bytes());
                
                let packet = RtpPacket { header: RtpHeader::new(dtmf_payload_type, seq, ts, ssrc), payload };
                let _ = socket.send_to(&packet.to_bytes(), target);
                
                if i < 4 { dtmf_duration += 160; }
                seq = seq.wrapping_add(1);
                std::thread::sleep(std::time::Duration::from_millis(20)); 
            }
            ts = ts.wrapping_add(dtmf_duration as u32);
            continue; 
        }

        // TX
        if mic_cons.len() >= hw_frame_size {
            let mut mic_data = Vec::with_capacity(hw_frame_size);
            for _ in 0..hw_frame_size {
                mic_data.push((mic_cons.pop().unwrap_or(0.0).clamp(-1.0, 1.0) * 32767.0) as i16);
            }

            let resampled = simple_resample(&mic_data, hw_sample_rate_in, 8000);
            for chunk in resampled.chunks(target_8k_samples) {
                if chunk.len() < target_8k_samples { continue; }
                let payload = encoder.encode(chunk);
                if payload.is_empty() { continue; }

                let packet = RtpPacket { header: RtpHeader::new(payload_type, seq, ts, ssrc), payload };
                if socket.send_to(&packet.to_bytes(), target).is_ok() {
                    tx_cnt.fetch_add(1, Ordering::Relaxed);
                }
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(target_8k_samples as u32);
            }
        }

        // RX
        while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
            if len > 12 {
                rx_cnt.fetch_add(1, Ordering::Relaxed);
                let samples_8k = decoder.decode(&recv_buf[12..len]);
                let resampled_out = simple_resample(&samples_8k, 8000, hw_sample_rate_out);
                
                for s in resampled_out { 
                    let _ = spk_prod.push(s as f32 / 32768.0); 
                }
            }
        }
    }
    Ok(())
}