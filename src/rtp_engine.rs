// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ringbuf::HeapRb;
use tracing::{info, error, debug};
use sentiric_rtp_core::{AudioProfile, CodecFactory, Pacer, RtpHeader, RtpPacket, simple_resample};
use std::panic;
// CPAL (Ses Kartı) sadece Hardware modunda kullanılacak
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct RtpEngine {
    socket: Arc<UdpSocket>,
    is_running: Arc<AtomicBool>,
    pub rx_count: Arc<AtomicU64>,
    pub tx_count: Arc<AtomicU64>,
    headless_mode: bool,
}

impl RtpEngine {
    pub fn new(socket: Arc<UdpSocket>, headless: bool) -> Self {
        Self {
            socket,
            is_running: Arc::new(AtomicBool::new(false)),
            rx_count: Arc::new(AtomicU64::new(0)),
            tx_count: Arc::new(AtomicU64::new(0)),
            headless_mode: headless,
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

        std::thread::Builder::new()
            .name("rtp-worker".to_string())
            .spawn(move || {
                let is_running_inner = is_running.clone();
                let result = panic::catch_unwind(move || {
                    if headless {
                        info!("👻 Starting HEADLESS Audio Loop (Virtual DSP)");
                        if let Err(e) = run_headless_loop(is_running_inner.clone(), socket, target, rx_cnt, tx_cnt) {
                            error!("Headless Loop Error: {:?}", e);
                        }
                    } else {
                        info!("🎤 Starting HARDWARE Audio Loop (CPAL)");
                        if let Err(e) = run_hardware_loop(is_running_inner.clone(), socket, target, rx_cnt, tx_cnt) {
                            error!("Hardware Loop Error: {:?}", e);
                        }
                    }
                    is_running_inner.store(false, Ordering::SeqCst);
                });
                
                if let Err(err) = result {
                    error!("RTP Thread Panic: {:?}", err);
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

    // [FIX]: Türü açıkça f32 olarak belirttik
    let mut phase: f32 = 0.0;
    let freq = 1000.0;
    let sample_rate = 8000.0;
    let step = freq * 2.0 * std::f32::consts::PI / sample_rate;

    while is_running.load(Ordering::SeqCst) {
        pacer.wait();

        // 1. Sanal Mikrofon
        let mut pcm_frame = Vec::with_capacity(sample_per_frame);
        for _ in 0..sample_per_frame {
            let val = (phase.sin() * 20000.0) as i16;
            pcm_frame.push(val);
            phase += step;
            if phase > 2.0 * std::f32::consts::PI { phase -= 2.0 * std::f32::consts::PI; }
        }

        // 2. Encode & Send
        let payload = encoder.encode(&pcm_frame);
        if !payload.is_empty() {
            let header = RtpHeader::new(payload_type, seq, ts, ssrc);
            let packet = RtpPacket { header, payload };
            
            match socket.try_send_to(&packet.to_bytes(), target) {
                Ok(_) => { tx_cnt.fetch_add(1, Ordering::Relaxed); },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                Err(e) => error!("Headless TX Error: {}", e),
            }
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(sample_per_frame as u32);
        }

        // 3. Receive & Analyze
        loop {
            match socket.try_recv_from(&mut recv_buf) {
                Ok((len, _)) => {
                    if len > 12 {
                        rx_cnt.fetch_add(1, Ordering::Relaxed);
                        // Decode edip ses var mı kontrol et (RMS)
                        let payload = &recv_buf[12..len];
                        let samples = decoder.decode(payload);
                        
                        let mut sum_sq = 0.0;
                        // [FIX]: Gereksiz parantez kaldırıldı
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
        .ok_or_else(|| anyhow::anyhow!("No Default Input Device Found"))?;
    let output_device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No Default Output Device Found"))?;

    info!("🎤 Input: {}", input_device.name().unwrap_or("Unknown".into()));
    info!("🔊 Output: {}", output_device.name().unwrap_or("Unknown".into()));

    let config: cpal::StreamConfig = input_device.default_input_config()?.into();
    let hw_sample_rate = config.sample_rate.0 as usize;

    let rb_in = HeapRb::<f32>::new(8192);
    let (mut mic_prod, mut mic_cons) = rb_in.split();
    let rb_out = HeapRb::<f32>::new(8192);
    let (mut spk_prod, mut spk_cons) = rb_out.split();

    let err_fn = |err| error!("Audio Stream Callback Error: {}", err);

    let input_stream = input_device.build_input_stream(
        &config, 
        move |data: &[f32], _: &_| {
            for &s in data { let _ = mic_prod.push(s); }
        }, 
        err_fn, 
        None
    )?;

    let output_config: cpal::StreamConfig = output_device.default_output_config()?.into();
    let output_stream = output_device.build_output_stream(
        &output_config, 
        move |data: &mut [f32], _: &_| {
            for s in data.iter_mut() { *s = spk_cons.pop().unwrap_or(0.0); }
        }, 
        err_fn, 
        None
    )?;

    input_stream.play()?;
    output_stream.play()?;
    
    // DSP & Codec Loop
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
                    Err(e) => error!("RTP Send Error: {}", e),
                }
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(sample_per_frame as u32);
            }
        }

        // RX
        loop {
             match socket.try_recv_from(&mut recv_buf) {
                 Ok((len, src)) => {
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