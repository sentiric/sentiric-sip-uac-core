// sentiric-telecom-client-sdk/src/rtp_engine.rs

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, AtomicU8, Ordering};
use tokio::sync::mpsc;
use std::panic;


use sentiric_rtp_core::{CodecFactory, CodecType, Pacer, RtpHeader, RtpPacket};
use crate::UacEvent;
use crate::media::{MediaAdapter, HardwareAdapter, HeadlessAdapter};

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
    is_muted: Arc<AtomicBool>, // MUTE KONTROLCÜSÜ
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
            is_muted: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn update_gains(&self, mic: f32, spk: f32) {
        self.mic_gain.store(mic.to_bits(), Ordering::Relaxed);
        self.speaker_gain.store(spk.to_bits(), Ordering::Relaxed);
    }

    pub fn set_mute(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    pub fn trigger_dtmf(&self, key: char) {
        let event_id = match key {
            '0'..='9' => (key as u8) - b'0',
            '*' => 10,
            '#' => 11,
            'A'..='D' => 12 + ((key as u8) - b'A'),
            _ => 255,
        };
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
        let is_muted = self.is_muted.clone();

        std::thread::Builder::new()
            .name("rtp-network-worker".to_string())
            .spawn(move || {
                let is_running_inner = is_running.clone();
                let ui_tx_inner = ui_tx.clone();
                
                let result = panic::catch_unwind(move || {
                    let _ = run_media_loop(
                        is_running_inner.clone(), socket, target, rx_cnt, tx_cnt, 
                        live_mic, live_spk, dtmf_queue, is_muted, headless, ui_tx_inner
                    );
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

// TEMİZ AĞ DÖNGÜSÜ (PURE NETWORK LOOP)
fn run_media_loop(
    is_running: Arc<AtomicBool>, socket: Arc<UdpSocket>, target: SocketAddr,
    rx_cnt: Arc<AtomicU64>, tx_cnt: Arc<AtomicU64>,
    live_mic_gain: Arc<AtomicU32>, live_speaker_gain: Arc<AtomicU32>,
    dtmf_queue: Arc<AtomicU8>, is_muted: Arc<AtomicBool>, headless: bool,
    ui_tx: mpsc::Sender<UacEvent>
) -> anyhow::Result<()> {
    
    // [SELF-HEALING OUTER LOOP]: Adaptör çökerse (Rota Değişirse) buradan yeniden kurulur.
    while is_running.load(Ordering::SeqCst) {
        
        // 1. ADAPTÖRÜ SEÇ VE BAŞLAT
        let adapter: Box<dyn MediaAdapter> = if headless {
            let _ = ui_tx.blocking_send(UacEvent::Log("👻 Booting Virtual DSP (Headless Mode)".into()));
            Box::new(HeadlessAdapter::new())
        } else {
            let _ = ui_tx.blocking_send(UacEvent::Log("🎤 Booting Hardware Audio Engine...".into()));
            match HardwareAdapter::new() {
                Ok(a) => Box::new(a),
                Err(e) => {
                    let _ = ui_tx.blocking_send(UacEvent::Log(format!("⚠️ Hardware Init Fail: {}", e)));
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            }
        };

        // 2. NETWORK ENGINE SETUP
        let codec_type = CodecType::PCMU;
        let mut encoder = CodecFactory::create_encoder(codec_type);
        let mut decoder = CodecFactory::create_decoder(codec_type);
        let mut pacer = Pacer::new(20);
        
        let mut seq: u16 = rand::random();
        let mut ts: u32 = rand::random();
        let ssrc: u32 = rand::random();
        let mut recv_buf = [0u8; 1500];

        pacer.reset(); 

        // [STRICT INNER LOOP] Sadece 20ms'lik Ağ İşlemleri
        while is_running.load(Ordering::SeqCst) && adapter.is_healthy() {
            pacer.wait();

            // A. Kontrolleri Senkronize Et
            let mg = f32::from_bits(live_mic_gain.load(Ordering::Relaxed));
            let sg = f32::from_bits(live_speaker_gain.load(Ordering::Relaxed));
            adapter.update_gains(mg, sg);
            adapter.set_mute(is_muted.load(Ordering::Relaxed));

            // B. DTMF Kontrolü
            let dtmf_event = dtmf_queue.swap(255, Ordering::Relaxed);
            if dtmf_event != 255 {
                let mut dtmf_duration: u16 = 160;
                for i in 0..5 {
                    let end_bit = if i == 4 { 0x80 } else { 0x00 };
                    let payload = vec![dtmf_event, end_bit | 10, (dtmf_duration >> 8) as u8, dtmf_duration as u8];
                    let packet = RtpPacket { header: RtpHeader::new(101, seq, ts, ssrc), payload };
                    let _ = socket.send_to(&packet.to_bytes(), target);
                    
                    if i < 4 { dtmf_duration += 160; }
                    seq = seq.wrapping_add(1);
                    std::thread::sleep(std::time::Duration::from_millis(20)); 
                }
                ts = ts.wrapping_add(dtmf_duration as u32);
                continue; 
            }

            // C. RX (Ağdan Oku -> Hoparlöre Yaz)
            while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
                if len > 12 {
                    rx_cnt.fetch_add(1, Ordering::Relaxed);
                    let payload_type = recv_buf[1] & 0x7F;
                    if payload_type != 101 { // DTMF değilse ses verisidir
                        let samples_8k = decoder.decode(&recv_buf[12..len]);
                        adapter.write_spk(&samples_8k);
                    }
                }
            }

            // D. TX (Mikrofondan Oku -> Ağa Yaz)
            let mic_data = adapter.read_mic(160);
            if mic_data.len() == 160 {
                let payload = encoder.encode(&mic_data);
                if !payload.is_empty() {
                    let packet = RtpPacket { header: RtpHeader::new(0, seq, ts, ssrc), payload };
                    if socket.send_to(&packet.to_bytes(), target).is_ok() {
                        tx_cnt.fetch_add(1, Ordering::Relaxed);
                    }
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(160);
                }
            }
        } // İç Döngü Bitişi

        if is_running.load(Ordering::SeqCst) {
            let _ = ui_tx.blocking_send(UacEvent::Log("🔄 Route Change Detected! Rebuilding streams...".into()));
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    Ok(())
}