// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::{SocketAddr, UdpSocket}; // std::net kullanılıyor
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ringbuf::HeapRb;
use tracing::{info, error, debug};
use sentiric_rtp_core::{AudioProfile, CodecFactory, Pacer, RtpHeader, RtpPacket, simple_resample};
use std::panic;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;
use crate::UacEvent; 

pub struct RtpEngine {
    socket: Arc<UdpSocket>, // Artık std soketi
    is_running: Arc<AtomicBool>,
    pub rx_count: Arc<AtomicU64>,
    pub tx_count: Arc<AtomicU64>,
    headless_mode: bool,
    event_tx: mpsc::Sender<UacEvent>,
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
        }
    }

    pub fn start(&self, target: SocketAddr) {
        if self.is_running.swap(true, Ordering::SeqCst) { 
            return; 
        }
        
        let is_running = self.is_running.clone();
        let socket = self.socket.clone();
        let rx_cnt = self.rx_count.clone();
        let tx_cnt = self.tx_count.clone();
        let headless = self.headless_mode;
        let ui_tx = self.event_tx.clone();

        std::thread::Builder::new()
            .name("rtp-worker".to_string())
            .spawn(move || {
                let is_running_inner = is_running.clone();
                let ui_tx_inner = ui_tx.clone();
                
                let result = panic::catch_unwind(move || {
                    if headless {
                        let _ = ui_tx_inner.blocking_send(UacEvent::Log("👻 Booting Virtual DSP (Headless)".into()));
                        if let Err(e) = run_headless_loop(is_running_inner.clone(), socket, target, rx_cnt, tx_cnt) {
                            let _ = ui_tx_inner.blocking_send(UacEvent::Error(format!("Virtual DSP Error: {}", e)));
                        }
                    } else {
                        let _ = ui_tx_inner.blocking_send(UacEvent::Log("🎤 Connecting to Hardware Mic/Speaker...".into()));
                        
                        if let Err(e) = run_hardware_loop(is_running_inner.clone(), socket.clone(), target, rx_cnt.clone(), tx_cnt.clone()) {
                            
                            let err_msg = format!("⚠️ Hardware Audio Failed: {}. FALLING BACK TO VIRTUAL AUDIO!", e);
                            let _ = ui_tx_inner.blocking_send(UacEvent::Log(err_msg));
                            
                            if let Err(fallback_err) = run_headless_loop(is_running_inner.clone(), socket, target, rx_cnt, tx_cnt) {
                                let _ = ui_tx_inner.blocking_send(UacEvent::Error(format!("Fallback also failed: {}", fallback_err)));
                            }
                        }
                    }
                    is_running_inner.store(false, Ordering::SeqCst);
                });
                
                if let Err(err) = result {
                    let msg = if let Some(s) = err.downcast_ref::<&str>() { s.to_string() } 
                              else if let Some(s) = err.downcast_ref::<String>() { s.clone() } 
                              else { "Unknown panic reason".to_string() };
                    
                    let _ = ui_tx.blocking_send(UacEvent::Error(format!("☠️ CRITICAL: RTP Thread Panicked! Reason: {}", msg)));
                    is_running.store(false, Ordering::SeqCst);
                }
            })
            .expect("Failed to spawn RTP thread");
    }

    pub fn stop(&self) {
        self.is_running.store(false, Ordering::SeqCst);
    }
}

// --- SANAL (HEADLESS) AUDIO LOOP ---
fn run_headless_loop(
    is_running: Arc<AtomicBool>, 
    socket: Arc<UdpSocket>, 
    target: SocketAddr,
    rx_cnt: Arc<AtomicU64>,
    tx_cnt: Arc<AtomicU64>
) -> anyhow::Result<()> {
    let profile = AudioProfile::default();
    let codec_type = profile.preferred_audio_codec();
    let payload_type = profile.get_by_payload(codec_type as u8).map(|c| c.payload_type).unwrap_or(0);

    let mut encoder = CodecFactory::create_encoder(codec_type);
    let mut decoder = CodecFactory::create_decoder(codec_type);
    let mut pacer = Pacer::new(profile.ptime as u64);
    
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = 0xDEADBEEF; 
    let sample_per_frame = codec_type.samples_per_frame(profile.ptime);
    let mut recv_buf = [0u8; 1500];

    let mut phase: f32 = 0.0;
    let freq = 1000.0;
    let sample_rate = 8000.0;
    let step = freq * 2.0 * std::f32::consts::PI / sample_rate;
    
    // NAT Hole Punching
    for _ in 0..10 {
        if !is_running.load(Ordering::SeqCst) { break; }
        let silence_frame = vec![0i16; sample_per_frame];
        let payload = encoder.encode(&silence_frame);
        let header = RtpHeader::new(payload_type, seq, ts, ssrc);
        let packet = RtpPacket { header, payload };
        let _ = socket.send_to(&packet.to_bytes(), target); // std::net method
        std::thread::sleep(std::time::Duration::from_millis(20));
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(sample_per_frame as u32);
    }

    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        let mut pcm_frame = Vec::with_capacity(sample_per_frame);
        for _ in 0..sample_per_frame {
            let val = (phase.sin() * 20000.0) as i16;
            pcm_frame.push(val);
            phase += step;
            if phase > 2.0 * std::f32::consts::PI { phase -= 2.0 * std::f32::consts::PI; }
        }

        let payload = encoder.encode(&pcm_frame);
        if !payload.is_empty() {
            let header = RtpHeader::new(payload_type, seq, ts, ssrc);
            let packet = RtpPacket { header, payload };
            
            match socket.send_to(&packet.to_bytes(), target) {
                Ok(_) => { tx_cnt.fetch_add(1, Ordering::Relaxed); },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                Err(_) => {},
            }
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(sample_per_frame as u32);
        }

        loop {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, _)) => {
                    if len > 12 {
                        rx_cnt.fetch_add(1, Ordering::Relaxed);
                        let payload = &recv_buf[12..len];
                        let samples = decoder.decode(payload);
                        
                        let mut sum_sq = 0.0;
                        for s in &samples { sum_sq += *s as f32 * *s as f32; }
                        let rms = (sum_sq / samples.len() as f32).sqrt();

                        if rms > 100.0 && (seq % 100 == 0) {
                            debug!("🔊 Headless RX: Signal Detected. RMS: {:.2}", rms);
                        }
                    }
                },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
    Ok(())
}

// --- DONANIM (HARDWARE) LOOP ---
fn run_hardware_loop(
    is_running: Arc<AtomicBool>, 
    socket: Arc<UdpSocket>, 
    target: SocketAddr,
    rx_cnt: Arc<AtomicU64>,
    tx_cnt: Arc<AtomicU64>
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    
    let input_device = host.default_input_device()
        .ok_or_else(|| anyhow::anyhow!("Microphone access denied or not found by OS"))?;
    
    let output_device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("Speaker access denied or not found by OS"))?;

    // [FIX]: Esnek Kanal Seçimi (Adaptive Channel Selection)
    // Önce Mono (1ch) ara, bulamazsan Stereo (2ch) kullan ve yazılımla çevir.
    
    // --- INPUT CONFIG ---
    let supported_inputs = input_device.supported_input_configs()?;
    let mut best_input_config = None;
    
    // 1. Tercih: Mono (En iyi performans ve AEC için)
    for cfg in supported_inputs {
        if cfg.channels() == 1 {
            best_input_config = Some(cfg);
            break; // Bulduk!
        }
        // Mono yoksa Stereo'yu sakla (Fallback olarak)
        if best_input_config.is_none() && cfg.channels() == 2 {
            best_input_config = Some(cfg);
        }
    }
    
    let input_config_range = best_input_config.ok_or_else(|| anyhow::anyhow!("No supported input config found"))?;
    let input_config: cpal::StreamConfig = input_config_range.with_max_sample_rate().into();
    let in_channels = input_config.channels as usize;

    // --- OUTPUT CONFIG ---
    let supported_outputs = output_device.supported_output_configs()?;
    let mut best_output_config = None;
    
    for cfg in supported_outputs {
        if cfg.channels() == 1 {
            best_output_config = Some(cfg);
            break; 
        }
        if best_output_config.is_none() && cfg.channels() == 2 {
            best_output_config = Some(cfg);
        }
    }

    let output_config_range = best_output_config.ok_or_else(|| anyhow::anyhow!("No supported output config found"))?;
    let output_config: cpal::StreamConfig = output_config_range.with_max_sample_rate().into();
    let out_channels = output_config.channels as usize;

    let hw_sample_rate_in = input_config.sample_rate.0 as usize;
    let hw_sample_rate_out = output_config.sample_rate.0 as usize;
    
    info!("⚙️ Hardware Audio Config [ADAPTIVE] - In: {}Hz {}ch | Out: {}Hz {}ch", 
        hw_sample_rate_in, in_channels, hw_sample_rate_out, out_channels);

    // Buffer'ı 1 saniyelik veri alacak şekilde genişletelim
    let rb_in = HeapRb::<f32>::new(hw_sample_rate_in * 4); // x4 güvenlik
    let (mut mic_prod, mut mic_cons) = rb_in.split();
    let rb_out = HeapRb::<f32>::new(hw_sample_rate_out * 4);
    let (mut spk_prod, mut spk_cons) = rb_out.split();

    let err_fn = |err| error!("Audio Stream Error: {}", err);

    // [KRİTİK FIX]: Stereo -> Mono Downmix
    let input_stream = input_device.build_input_stream(
        &input_config, 
        move |data: &[f32], _: &_| {
            if in_channels == 1 {
                // Zaten Mono, olduğu gibi bas
                for &sample in data {
                    let _ = mic_prod.push(sample);
                }
            } else {
                // Stereo (veya çok kanallı) ise sadece ilk kanalı al (Sol)
                // Bu işlem "Interleaved" veriyi "Mono"ya çevirir.
                for frame in data.chunks(in_channels) {
                    if let Some(&sample) = frame.first() {
                        let _ = mic_prod.push(sample);
                    }
                }
            }
        }, 
        err_fn, 
        None
    ).map_err(|e| anyhow::anyhow!("build_input_stream failed: {}", e))?;

    let output_stream = output_device.build_output_stream(
        &output_config, 
        move |data: &mut [f32], _: &_| {
            // Mono -> Stereo (veya çok kanallı) Upmix
            for frame in data.chunks_mut(out_channels) {
                let sample = spk_cons.pop().unwrap_or(0.0);
                for s in frame.iter_mut() {
                    *s = sample; 
                }
            }
        }, 
        err_fn, 
        None
    ).map_err(|e| anyhow::anyhow!("build_output_stream failed: {}", e))?;

    input_stream.play().map_err(|e| anyhow::anyhow!("input_stream.play() failed: {}", e))?;
    output_stream.play().map_err(|e| anyhow::anyhow!("output_stream.play() failed: {}", e))?;
    
    let profile = AudioProfile::default();
    let codec_type = profile.preferred_audio_codec();
    let payload_type = profile.get_by_payload(codec_type as u8).map(|c| c.payload_type).unwrap_or(0);

    let mut encoder = CodecFactory::create_encoder(codec_type);
    let mut decoder = CodecFactory::create_decoder(codec_type);
    let mut pacer = Pacer::new(profile.ptime as u64);
    
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = rand::random();
    let target_8k_samples = codec_type.samples_per_frame(profile.ptime);
    
    // Chunk boyutu hesaplama (Mono'ya indirgendiği için channel çarpanı yok)
    let hw_frame_size = (hw_sample_rate_in * profile.ptime as usize) / 1000;
    
    let mut recv_buf = [0u8; 1500];

    // NAT Hole Punching
    for _ in 0..10 {
        if !is_running.load(Ordering::SeqCst) { break; }
        let silence_frame = vec![0i16; target_8k_samples];
        let payload = encoder.encode(&silence_frame);
        let header = RtpHeader::new(payload_type, seq, ts, ssrc);
        let packet = RtpPacket { header, payload };
        let _ = socket.send_to(&packet.to_bytes(), target);
        std::thread::sleep(std::time::Duration::from_millis(20));
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(target_8k_samples as u32);
    }
    info!("🕳️ NAT Hole Punching sequence sent. Media flowing...");

    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        // --- TX (Mikrofon -> Ağ) ---
        // Yeterli sample biriktiğinde işlemi yap
        if mic_cons.len() >= hw_frame_size {
            let mut mic_data = Vec::with_capacity(hw_frame_size);
            for _ in 0..hw_frame_size {
                let s = mic_cons.pop().unwrap_or(0.0);
                mic_data.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
            }

            let resampled = simple_resample(&mic_data, hw_sample_rate_in, 8000);
            
            for chunk in resampled.chunks(target_8k_samples) {
                if chunk.len() < target_8k_samples { continue; }
                
                let payload = encoder.encode(chunk);
                if payload.is_empty() { continue; }

                let header = RtpHeader::new(payload_type, seq, ts, ssrc);
                let packet = RtpPacket { header, payload };
                
                match socket.send_to(&packet.to_bytes(), target) {
                    Ok(_) => { tx_cnt.fetch_add(1, Ordering::Relaxed); },
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                    Err(_) => {},
                }
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(target_8k_samples as u32);
            }
        }

        // --- RX (Ağ -> Hoparlör) ---
        loop {
             match socket.recv_from(&mut recv_buf) {
                 Ok((len, _src)) => {
                     if len > 12 {
                         rx_cnt.fetch_add(1, Ordering::Relaxed);
                         let payload = &recv_buf[12..len];
                         let samples_8k = decoder.decode(payload);
                         
                         let resampled_out = simple_resample(&samples_8k, 8000, hw_sample_rate_out);
                         for s in resampled_out { 
                             let _ = spk_prod.push(s as f32 / 32768.0); 
                         }
                     }
                 },
                 Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                 Err(_) => break,
             }
        }
    }
    
    Ok(())
}