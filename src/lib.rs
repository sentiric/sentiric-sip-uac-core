// sentiric-sip-uac-core/src/lib.rs

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, parser};
use sentiric_rtp_core::{RtpHeader, RtpPacket, CodecFactory, Pacer, AudioProfile};
use rand::Rng;

/// İstemci (CLI/Mobile) tarafından dinlenecek olaylar.
#[derive(Debug, Clone)]
pub enum UacEvent {
    Log(String),          // Genel bilgilendirme (DEBUG/INFO)
    Status(String),       // Kullanıcıya gösterilecek durum (Ringing, Connected)
    Error(String),        // Hata mesajı
    CallEnded,            // Çağrı bitti
}

/// Çekirdek SIP İstemcisi
pub struct UacClient {
    event_tx: mpsc::Sender<UacEvent>,
}

impl UacClient {
    /// Yeni bir istemci oluşturur. Olaylar `event_tx` kanalına gönderilir.
    pub fn new(event_tx: mpsc::Sender<UacEvent>) -> Self {
        Self { event_tx }
    }

    /// Yardımcı: Olay gönderme
    async fn emit(&self, event: UacEvent) {
        let _ = self.event_tx.send(event).await;
    }

    /// Bir çağrı başlatır ve RTP akışını yönetir.
    pub async fn start_call(&self, target_ip: String, target_port: u16, to_user: String, from_user: String) -> anyhow::Result<()> {
        let local_port = 0; // 0 = İşletim sistemi rastgele boş port verir (Mobilde 6060 dolu olabilir)
        
        // Soket Bağlantısı
        let socket = match UdpSocket::bind(format!("0.0.0.0:{}", local_port)).await {
            Ok(s) => s,
            Err(e) => {
                self.emit(UacEvent::Error(format!("Socket bind failed: {}", e))).await;
                return Err(e.into());
            }
        };

        // Gerçek atanan portu öğren
        let local_addr = socket.local_addr()?;
        let bound_port = local_addr.port();
        
        self.emit(UacEvent::Log(format!("Socket bound to local port: {}", bound_port))).await;

        let target_addr_str = format!("{}:{}", target_ip, target_port);
        let target_addr: SocketAddr = target_addr_str.parse()?;

        // --- SIP INVITE Hazırlığı ---
        let call_id = format!("uac-core-{}", rand::thread_rng().gen::<u32>());
        let from = format!("<sip:{}@sentiric.mobile>;tag=uac-tag", from_user);
        let to = format!("<sip:{}@{}>", to_user, target_ip);
        
        let mut invite = SipPacket::new_request(
            Method::Invite, 
            format!("sip:{}@{}:{}", to_user, target_ip, target_port)
        );
        
        // IP tespiti mobilde zor olabilir, şimdilik 127.0.0.1 yerine 0.0.0.0 veya soft-coded bir değer kullanıyoruz.
        // İdealde STUN gerekir ama test için bu yeterli.
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-{}", bound_port, rand::thread_rng().gen::<u16>())));
        invite.headers.push(Header::new(HeaderName::From, from.clone()));
        invite.headers.push(Header::new(HeaderName::To, to.clone()));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@0.0.0.0:{}>", from_user, bound_port)));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-UAC-Core/1.0".to_string()));

        // SDP (PCMU Only)
        let sdp = format!("v=0\r\no=- 123 123 IN IP4 0.0.0.0\r\ns=SentiricMobile\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\na=ptime:20\r\n", bound_port+2); // RTP port tahmini
        invite.body = sdp.as_bytes().to_vec();

        self.emit(UacEvent::Status("Sending INVITE...".into())).await;
        socket.send_to(&invite.to_bytes(), target_addr).await?;

        // --- Response Loop ---
        let mut buf = [0u8; 4096];
        loop {
            let (size, src) = socket.recv_from(&mut buf).await?;
            let packet = match parser::parse(&buf[..size]) {
                Ok(p) => p,
                Err(_) => continue, // Bozuk paket
            };

            if packet.status_code >= 100 && packet.status_code < 200 {
                self.emit(UacEvent::Status(format!("Server: {} {}", packet.status_code, packet.reason))).await;
            } else if packet.status_code >= 200 && packet.status_code < 300 {
                self.emit(UacEvent::Status("Call Answered! (200 OK)".into())).await;
                
                // ACK Gönder
                let remote_tag = packet.get_header_value(HeaderName::To).cloned().unwrap_or(to.clone());
                let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", src));
                ack.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
                ack.headers.push(Header::new(HeaderName::From, from.clone()));
                ack.headers.push(Header::new(HeaderName::To, remote_tag));
                ack.headers.push(Header::new(HeaderName::CSeq, "1 ACK".to_string()));
                ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-ack", bound_port)));
                socket.send_to(&ack.to_bytes(), target_addr).await?;

                // RTP Başlat (Basit Dummy Stream)
                // Gerçek hedef portu SDP'den almak gerekir ama şimdilik sunucunun gönderdiği adrese (simetrik) güvenelim
                // veya basitçe +2 port varsayalım (Geliştirilecek)
                let rtp_target = SocketAddr::new(target_addr.ip(), 30000); // Varsayılan SBC RTP

                self.emit(UacEvent::Status("Streaming Audio...".into())).await;
                self.run_rtp_sequence(&socket, rtp_target).await?;
                break;
            } else if packet.status_code >= 300 {
                self.emit(UacEvent::Error(format!("Call Failed: {}", packet.status_code))).await;
                return Err(anyhow::anyhow!("SIP Error"));
            }
        }
        
        self.emit(UacEvent::CallEnded).await;
        Ok(())
    }

    async fn run_rtp_sequence(&self, socket: &UdpSocket, target: SocketAddr) -> anyhow::Result<()> {
        let profile = AudioProfile::default();
        let codec_type = profile.preferred_audio_codec();
        let mut encoder = CodecFactory::create_encoder(codec_type);
        let mut pacer = Pacer::new(profile.ptime as u64);
        let ssrc: u32 = rand::thread_rng().gen();
        let mut seq: u16 = 0;
        let mut ts: u32 = 0;
        let payload_size = 160;

        // 5 Saniyelik ses gönder (Test için yeterli)
        for _ in 0..250 {
            pacer.wait();
            let pcm = vec![0i16; payload_size]; 
            let payload = encoder.encode(&pcm);
            let mut header = RtpHeader::new(codec_type as u8, seq, ts, ssrc);
            let rtp_pkt = RtpPacket { header, payload };
            
            let _ = socket.send_to(&rtp_pkt.to_bytes(), target).await;
            
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(payload_size as u32);
        }
        Ok(())
    }
}