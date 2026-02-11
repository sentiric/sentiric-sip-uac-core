// sentiric-sip-uac-core/src/lib.rs

use std::net::SocketAddr;
use std::sync::Arc; 
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, parser};
use sentiric_rtp_core::{RtpHeader, RtpPacket, CodecFactory, Pacer, AudioProfile, AudioResampler};
use rand::Rng;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;

/// İstemci (CLI/Mobile) tarafından dinlenecek olaylar.
#[derive(Debug, Clone)]
pub enum UacEvent {
    Log(String),          // Teknik loglar
    Status(String),       // Kullanıcı dostu durum mesajları
    Error(String),        // Hata durumları
    CallEnded,            // Çağrı sonlanma sinyali
}

/// Sentiric SIP İstemci Çekirdeği
pub struct UacClient {
    event_tx: mpsc::Sender<UacEvent>,
}

impl UacClient {
    pub fn new(event_tx: mpsc::Sender<UacEvent>) -> Self {
        Self { event_tx }
    }

    /// Olayları asenkron kanala güvenli bir şekilde basar.
    async fn emit(&self, event: UacEvent) {
        let _ = self.event_tx.send(event).await;
    }

    /// Belirtilen hedefe SIP çağrısı başlatır ve başarılı olursa donanım sesini açar.
    pub async fn start_call(&self, target_ip: String, target_port: u16, to_user: String, from_user: String) -> anyhow::Result<()> {
        // İşletim sisteminden rastgele bir UDP portu al (0 verilerek OS'e bırakılır)
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let bound_port = socket.local_addr()?.port();
        let target_addr: SocketAddr = format!("{}:{}", target_ip, target_port).parse()?;

        // --- SIP INVITE OLUŞTURMA ---
        // RFC 3261 standartlarına uygun başlıklar
        let call_id = format!("uac-hw-{}", rand::thread_rng().gen::<u32>());
        let from = format!("<sip:{}@sentiric.mobile>;tag=uac-hw-tag", from_user);
        let to = format!("<sip:{}@{}>", to_user, target_ip);
        
        let mut invite = SipPacket::new_request(
            Method::Invite, 
            format!("sip:{}@{}:{}", to_user, target_ip, target_port)
        );
        
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-{}", bound_port, rand::thread_rng().gen::<u16>())));
        invite.headers.push(Header::new(HeaderName::From, from.clone()));
        invite.headers.push(Header::new(HeaderName::To, to.clone()));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@0.0.0.0:{}>", from_user, bound_port)));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-UAC-Hardware/1.0".to_string()));

        // SDP Tanımı: Sadece PCMU (8000Hz) kabul ediyoruz.
        // Port tahmini (RTP portu sinyal portundan farklı olmalı)
        let sdp = format!("v=0\r\no=- 123 123 IN IP4 0.0.0.0\r\ns=SentiricHW\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\na=ptime:20\r\n", bound_port+2);
        invite.body = sdp.as_bytes().to_vec();

        self.emit(UacEvent::Status("Dialing Hardware...".into())).await;
        socket.send_to(&invite.to_bytes(), target_addr).await?;

        // Yanıt Bekleme Döngüsü
        let mut buf = [0u8; 4096];
        loop {
            let (size, src) = socket.recv_from(&mut buf).await?;
            let packet = match parser::parse(&buf[..size]) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if packet.status_code == 200 {
                self.emit(UacEvent::Status("CONNECTED (Hardware Active)".into())).await;
                
                // SIP ACK: Üçlü el sıkışmayı tamamla
                let remote_tag = packet.get_header_value(HeaderName::To).cloned().unwrap_or(to.clone());
                let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", src));
                ack.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
                ack.headers.push(Header::new(HeaderName::From, from.clone()));
                ack.headers.push(Header::new(HeaderName::To, remote_tag));
                ack.headers.push(Header::new(HeaderName::CSeq, "1 ACK".to_string()));
                ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-ack", bound_port)));
                socket.send_to(&ack.to_bytes(), target_addr).await?;

                // --- HARDWARE AUDIO STREAM BAŞLAT ---
                // Sentiric SBC standart olarak 30000 portundan RTP dinler.
                let rtp_target = SocketAddr::new(target_addr.ip(), 30000);
                self.run_hardware_audio_stream(socket, rtp_target).await?;
                break;
            } else if packet.status_code >= 300 {
                self.emit(UacEvent::Error(format!("Call Rejected: {}", packet.status_code))).await;
                break;
            }
        }
        
        self.emit(UacEvent::CallEnded).await;
        Ok(())
    }

    /// Mikrofon verilerini RTP paketlerine çevirir ve gelen paketleri hoparlöre verir.
    async fn run_hardware_audio_stream(&self, socket: UdpSocket, target: SocketAddr) -> anyhow::Result<()> {
        let host = cpal::default_host();
        let input_device = host.default_input_device().ok_or_else(|| anyhow::anyhow!("Microphone not found"))?;
        let output_device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("Speaker not found"))?;

        let config: cpal::StreamConfig = input_device.default_input_config()?.into();
        let sample_rate = config.sample_rate.0 as usize;

        // 1. MİKROFON BUFFER (RingBuffer)
        // --------------------------------
        // Mikrofon callback'inden (Producer) veriyi alıp ana döngüye (Consumer) taşır.
        let rb = HeapRb::<f32>::new(sample_rate * 2);
        let (mut mic_prod, mut mic_cons) = rb.split();

        let input_stream = input_device.build_input_stream(
            &config,
            move |data: &[f32], _: &_| {
                for &sample in data { 
                    // RingBuffer'a veriyi it. Yer yoksa atılır (gerçek zamanlı ses için doğru yaklaşım).
                    let _ = mic_prod.push(sample); 
                }
            },
            |err| eprintln!("Mic error: {}", err),
            None
        )?;

        // 2. HOPARLÖR BUFFER (RingBuffer)
        // --------------------------------
        // Ana döngüden gelen veriyi (Producer) hoparlör callback'ine (Consumer) taşır.
        let out_rb = HeapRb::<f32>::new(sample_rate * 2);
        let (mut spk_prod, mut spk_cons) = out_rb.split();

        let output_stream = output_device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &_| {
                for sample in data.iter_mut() {
                    // Buffer'dan veri al, yoksa sessizlik (0.0) çal.
                    *sample = spk_cons.pop().unwrap_or(0.0);
                }
            },
            |err| eprintln!("Speaker error: {}", err),
            None
        )?;

        // Donanımı fiziksel olarak başlat
        input_stream.play()?;
        output_stream.play()?;

        // DSP ve Resampling Araçları (Iron Core üzerinden)
        let profile = AudioProfile::default();
        let mic_resampler = AudioResampler::new(sample_rate, 8000, 480);
        let speaker_resampler = AudioResampler::new(8000, sample_rate, 160);
        let mut encoder = CodecFactory::create_encoder(profile.preferred_audio_codec());
        let mut decoder = CodecFactory::create_decoder(profile.preferred_audio_codec());

        let arc_socket = Arc::new(socket);
        let rtp_ssrc: u32 = rand::random();
        let mut rtp_seq: u16 = 0;
        let mut rtp_ts: u32 = 0;
        let mut recv_buf = [0u8; 2048];
        let mut pacer = Pacer::new(20); // 20ms paket zamanlaması

        loop {
            pacer.wait();

            // A. Mikrofon -> RTP (TX)
            // -----------------------
            let mut mic_samples = Vec::new();
            while let Some(s) = mic_cons.pop() { 
                mic_samples.push((s * 32767.0) as i16); 
            }
            
            if !mic_samples.is_empty() {
                // Donanım hızından (örn: 48k) -> 8k'ya dönüştür.
                let resampled = mic_resampler.process(&mic_samples).await;
                // Codec (PCMU) ile sıkıştır.
                let payload = encoder.encode(&resampled);
                if !payload.is_empty() {
                    let header = RtpHeader::new(0, rtp_seq, rtp_ts, rtp_ssrc);
                    let pkt = RtpPacket { header, payload };
                    let _ = arc_socket.send_to(&pkt.to_bytes(), target).await;
                    rtp_seq = rtp_seq.wrapping_add(1);
                    rtp_ts = rtp_ts.wrapping_add(160);
                }
            }

            // B. Al -> Decode -> Hoparlör (RX)
            // --------------------------------
            // Ağdan gelen paketleri asenkron (non-blocking) kontrol et.
            match arc_socket.try_recv_from(&mut recv_buf) {
                Ok((size, _)) => {
                    if let Ok(pkt) = parser::parse(&recv_buf[..size]) {
                        // Sıkıştırılmış RTP payload'ını aç.
                        let samples_8k = decoder.decode(&pkt.body);
                        // 8k -> Donanım hızına (örn: 48k) dönüştür.
                        let resampled = speaker_resampler.process(&samples_8k).await;
                        for s in resampled { 
                            let _ = spk_prod.push(s as f32 / 32768.0); 
                        }
                    }
                },
                _ => {} // Henüz yeni paket yok.
            }
        }
    }
}