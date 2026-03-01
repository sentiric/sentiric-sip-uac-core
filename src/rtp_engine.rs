// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ringbuf::HeapRb;
use tracing::{info, error, warn, debug};
use sentiric_rtp_core::{AudioProfile, CodecFactory, Pacer, RtpHeader, RtpPacket, simple_resample};
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
                    // Panic nesnesini string'e çevirip basmaya çalışalım ki neden çöktüğünü görelim
                    let msg = if let Some(s) = err.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = err.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "Unknown panic reason".to_string()
                    };
                    
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
        let _ = socket.try_send_to(&packet.to_bytes(), target);
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
            
            match socket.try_send_to(&packet.to_bytes(), target) {
                Ok(_) => { tx_cnt.fetch_add(1, Ordering::Relaxed); },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                Err(_) => {},
            }
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(sample_per_frame as u32);
        }

        loop {
            match socket.try_recv_from(&mut recv_buf) {
                Ok((len, _)) => {
                    if len > 12 {
                        rx_cnt.fetch_add(1, Ordering::Relaxed);
                        let payload = &recv_buf[12..len];
                        let _ = decoder.decode(payload); 
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
        .ok_or_else(|| anyhow::anyhow!("Microphone access denied or not found by Android JNI"))?;
    
    let output_device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("Speaker access denied or not found"))?;

    // [FIX]: unwrap() KULLANIMI KALDIRILDI!
    let input_config = input_device.default_input_config()
        .map_err(|e| anyhow::anyhow!("Failed to get default input config: {}", e))?;
    let config: cpal::StreamConfig = input_config.into();
    let hw_sample_rate = config.sample_rate.0 as usize;

    let rb_in = HeapRb::<f32>::new(8192);
    let (mut mic_prod, mut mic_cons) = rb_in.split();
    let rb_out = HeapRb::<f32>::new(8192);
    let (mut spk_prod, mut spk_cons) = rb_out.split();

    let err_fn = |err| error!("Audio Stream Error: {}", err);

    let input_stream = input_device.build_input_stream(
        &config, 
        move |data: &[f32], _: &_| {
            for &s in data { let _ = mic_prod.push(s); }
        }, 
        err_fn, 
        None
    ).map_err(|e| anyhow::anyhow!("build_input_stream failed: {}", e))?;

    let output_config_raw = output_device.default_output_config()
        .map_err(|e| anyhow::anyhow!("Failed to get default output config: {}", e))?;
    let output_config: cpal::StreamConfig = output_config_raw.into();
        
    let output_stream = output_device.build_output_stream(
        &output_config, 
        move |data: &mut [f32], _: &_| {
            for s in data.iter_mut() { *s = spk_cons.pop().unwrap_or(0.0); }
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
    let sample_per_frame = codec_type.samples_per_frame(profile.ptime);
    let mut recv_buf = [0u8; 1500];

    // NAT Hole Punching
    for _ in 0..10 {
        if !is_running.load(Ordering::SeqCst) { break; }
        let silence_frame = vec![0i16; sample_per_frame];
        let payload = encoder.encode(&silence_frame);
        let header = RtpHeader::new(payload_type, seq, ts, ssrc);
        let packet = RtpPacket { header, payload };
        let _ = socket.try_send_to(&packet.to_bytes(), target);
        std::thread::sleep(std::time::Duration::from_millis(20));
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(sample_per_frame as u32);
    }

    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        // TX
        let mut mic_data = Vec::with_capacity(sample_per_frame * 2);
        while let Some(s) = mic_cons.pop() { 
            let s_clamped = s.clamp(-1.0, 1.0);
            mic_data.push((s_clamped * 32767.0) as i16); 
        }

        if !mic_data.is_empty() {
            let resampled = simple_resample(&mic_data, hw_sample_rate, 8000);
            for chunk in resampled.chunks(sample_per_frame) {
                if chunk.len() < sample_per_frame { continue; }
                let payload = encoder.encode(chunk);
                if payload.is_empty() { continue; }

                let header = RtpHeader::new(payload_type, seq, ts, ssrc);
                let packet = RtpPacket { header, payload };
                
                match socket.try_send_to(&packet.to_bytes(), target) {
                    Ok(_) => { tx_cnt.fetch_add(1, Ordering::Relaxed); },
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                    Err(_) => {},
                }
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(sample_per_frame as u32);
            }
        }

        // RX
        loop {
             match socket.try_recv_from(&mut recv_buf) {
                 Ok((len, src)) => {
                     // [FIX]: Hedef IP adresi ile eşleşiyor mu kontrolü
                     if src.ip() == target.ip() && len > 12 {
                         rx_cnt.fetch_add(1, Ordering::Relaxed);
                         let payload = &recv_buf[12..len];
                         let samples_8k = decoder.decode(payload);
                         let resampled_out = simple_resample(&samples_8k, 8000, hw_sample_rate);
                         for s in resampled_out { let _ = spk_prod.push(s as f32 / 32768.0); }
                     }
                 },
                 Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                 Err(_) => break,
             }
        }
    }
    
    Ok(())
}