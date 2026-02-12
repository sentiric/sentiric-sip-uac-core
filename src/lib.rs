// sentiric-sip-uac-core/src/lib.rs

use std::net::SocketAddr;
use tokio::net::UdpSocket as TokioUdpSocket;
use std::net::UdpSocket as StdUdpSocket; 
use tokio::sync::mpsc;
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, parser};
use sentiric_rtp_core::{RtpHeader, RtpPacket, CodecFactory, Pacer, AudioProfile, simple_resample, CodecType};
use rand::Rng;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;

/// İstemci (CLI/Mobile) tarafından dinlenecek olaylar.
#[derive(Debug, Clone)]
pub enum UacEvent {
    Log(String),
    Status(String),
    Error(String),
    CallEnded,
}

pub struct UacClient {
    event_tx: mpsc::Sender<UacEvent>,
}

impl UacClient {
    pub fn new(event_tx: mpsc::Sender<UacEvent>) -> Self {
        Self { event_tx }
    }

    async fn log_sip_packet(&self, packet: &SipPacket, direction: &str) {
        let summary = if packet.is_request {
            format!("{} {} {}", direction, packet.method, packet.uri)
        } else {
            format!("{} SIP {} {}", direction, packet.status_code, packet.reason)
        };
        let _ = self.event_tx.send(UacEvent::Log(summary)).await;
        
        if let Ok(raw_string) = String::from_utf8(packet.to_bytes()) {
             let _ = self.event_tx.send(UacEvent::Log(format!("-- RAW PACKET --\n{}\n----------------", raw_string))).await;
        }
    }

    // SDP'den IP ve Port çıkarma
    fn parse_sdp_target(&self, body: &[u8]) -> Option<SocketAddr> {
        let sdp_str = String::from_utf8_lossy(body);
        let mut ip = String::new();
        let mut port = 0u16;

        for line in sdp_str.lines() {
            if line.starts_with("c=IN IP4 ") {
                ip = line[9..].trim().to_string();
            } else if line.starts_with("m=audio ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() > 1 {
                    port = parts[1].parse().unwrap_or(0);
                }
            }
        }

        if !ip.is_empty() && port > 0 {
            if let Ok(addr) = format!("{}:{}", ip, port).parse() {
                return Some(addr);
            }
        }
        None
    }

    pub async fn start_call(&self, target_ip: String, target_port: u16, to_user: String, from_user: String) -> anyhow::Result<()> {
        let socket = TokioUdpSocket::bind("0.0.0.0:0").await?;
        let bound_port = socket.local_addr()?.port();
        let target_addr: SocketAddr = format!("{}:{}", target_ip, target_port).parse()?;

        // --- SIP INVITE ---
        let call_id = format!("uac-hw-{}", rand::thread_rng().gen::<u32>());
        let from = format!("<sip:{}@sentiric.mobile>;tag=uac-hw-tag", from_user);
        let to = format!("<sip:{}@{}>", to_user, target_ip);
        
        let mut invite = SipPacket::new_request(Method::Invite, format!("sip:{}@{}:{}", to_user, target_ip, target_port));
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-{}", bound_port, rand::thread_rng().gen::<u16>())));
        invite.headers.push(Header::new(HeaderName::From, from.clone()));
        invite.headers.push(Header::new(HeaderName::To, to.clone()));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@0.0.0.0:{}>", from_user, bound_port)));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-UAC-Hardware/1.0".to_string()));

        let sdp = format!("v=0\r\no=- 123 123 IN IP4 0.0.0.0\r\ns=SentiricHW\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\na=ptime:20\r\n", bound_port+2);
        invite.body = sdp.as_bytes().to_vec();
        
        self.log_sip_packet(&invite, "->").await;
        let _ = self.event_tx.send(UacEvent::Status("Dialing...".into())).await;
        
        socket.send_to(&invite.to_bytes(), target_addr).await?;

        let mut buf = [0u8; 4096];
        loop {
            let (size, src) = socket.recv_from(&mut buf).await?;
            let packet = match parser::parse(&buf[..size]) {
                Ok(p) => p,
                Err(e) => {
                    let _ = self.event_tx.send(UacEvent::Log(format!("Parse Error: {:?}", e))).await;
                    continue;
                },
            };

            self.log_sip_packet(&packet, "<-").await;

            if packet.status_code == 200 {
                let _ = self.event_tx.send(UacEvent::Status("CONNECTED (Hardware Active)".into())).await;
                
                // [KRİTİK GÜNCELLEME]: 200 OK içindeki SDP'den gerçek RTP hedefini al
                // Varsayılan olarak SIP IP'sini kullanırız ama port değişmiş olabilir (SBC Latching için)
                let rtp_remote_target = self.parse_sdp_target(&packet.body)
                    .unwrap_or_else(|| SocketAddr::new(target_addr.ip(), 30000)); // Fallback

                let _ = self.event_tx.send(UacEvent::Log(format!("🔊 RTP Target Locked: {}", rtp_remote_target))).await;

                let remote_tag = packet.get_header_value(HeaderName::To).cloned().unwrap_or(to.clone());
                // [FIX]: ACK, Contact header'ındaki URI'ye gönderilmelidir (Route set boşsa)
                // Ancak basitlik adına şimdilik gelen kaynağa (SBC) gönderiyoruz.
                let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", src));
                ack.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
                ack.headers.push(Header::new(HeaderName::From, from.clone()));
                ack.headers.push(Header::new(HeaderName::To, remote_tag));
                ack.headers.push(Header::new(HeaderName::CSeq, "1 ACK".to_string()));
                ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP 0.0.0.0:{};branch=z9hG4bK-ack", bound_port)));
                
                self.log_sip_packet(&ack, "->").await;
                socket.send_to(&ack.to_bytes(), target_addr).await?;

                let std_socket = socket.into_std()?;
                std_socket.set_nonblocking(false)?; 
                
                let event_tx_clone = self.event_tx.clone();

                std::thread::spawn(move || {
                    // RTP Target artık dinamik olarak parsed edilen adres
                    if let Err(e) = Self::run_hardware_audio_stream_sync(std_socket, rtp_remote_target, event_tx_clone.clone()) {
                        let _ = event_tx_clone.blocking_send(UacEvent::Error(format!("Audio Fail: {}", e)));
                    }
                    let _ = event_tx_clone.blocking_send(UacEvent::CallEnded);
                });
                
                break;
            }
        }
        Ok(())
    }

    // run_hardware_audio_stream_sync fonksiyonu aynı kalıyor...
    fn run_hardware_audio_stream_sync(socket: StdUdpSocket, target: SocketAddr, event_tx: mpsc::Sender<UacEvent>) -> anyhow::Result<()> {
        let host = cpal::default_host();
        let input_device = host.default_input_device().ok_or_else(|| anyhow::anyhow!("Mic not found"))?;
        let output_device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("Speaker not found"))?;

        let config: cpal::StreamConfig = input_device.default_input_config()?.into();
        let sample_rate = config.sample_rate.0 as usize;
        let _ = event_tx.blocking_send(UacEvent::Log(format!("Hardware Rate: {}Hz", sample_rate)));

        let rb = HeapRb::<f32>::new(sample_rate * 2);
        let (mut mic_prod, mut mic_cons) = rb.split();
        let _input_stream = input_device.build_input_stream(&config, move |data: &[f32], _| { for &sample in data { let _ = mic_prod.push(sample); } }, |err| eprintln!("Mic err: {}", err), None)?;

        let out_rb = HeapRb::<f32>::new(sample_rate * 2);
        let (mut spk_prod, mut spk_cons) = out_rb.split();
        let _output_stream = output_device.build_output_stream(&config, move |data: &mut [f32], _| { for sample in data.iter_mut() { *sample = spk_cons.pop().unwrap_or(0.0); } }, |err| eprintln!("Spk err: {}", err), None)?;

        _input_stream.play()?;
        _output_stream.play()?;

        let profile = AudioProfile::default();
        let mut encoder = CodecFactory::create_encoder(profile.preferred_audio_codec());
        let mut decoder = CodecFactory::create_decoder(profile.preferred_audio_codec());
        let mut pacer = Pacer::new(20);
        
        let rtp_ssrc: u32 = rand::random();
        let mut rtp_seq: u16 = 0;
        let mut rtp_ts: u32 = 0;
        let mut recv_buf = [0u8; 2048];

        let _ = event_tx.blocking_send(UacEvent::Log(format!("Media Loop Active. Codec: {:?}", profile.preferred_audio_codec())));

        let mut tx_count = 0;
        let mut rx_count = 0;
        let mut last_log = std::time::Instant::now();

        loop {
            pacer.wait();

            let mut mic_samples = Vec::new();
            while let Some(s) = mic_cons.pop() { mic_samples.push((s * 32767.0) as i16); }
            if !mic_samples.is_empty() {
                let resampled = simple_resample(&mic_samples, sample_rate, 8000);
                let payload = encoder.encode(&resampled);
                if !payload.is_empty() {
                    let pkt = RtpPacket { header: RtpHeader::new(0, rtp_seq, rtp_ts, rtp_ssrc), payload };
                    if let Err(e) = socket.send_to(&pkt.to_bytes(), target) {
                         let _ = event_tx.blocking_send(UacEvent::Error(format!("UDP Send Err: {}", e)));
                         break;
                    }
                    tx_count += 1;
                    rtp_seq = rtp_seq.wrapping_add(1);
                    rtp_ts = rtp_ts.wrapping_add(160);
                }
            }

            socket.set_nonblocking(true)?;
            if let Ok((size, src)) = socket.recv_from(&mut recv_buf) {
                // [FIX]: Gelen paketleri adres kontrolü yapmadan işle (NAT durumları için)
                // Yada en azından port değişse bile IP'nin aynı olduğunu kontrol et.
                if src.ip() == target.ip() {
                    rx_count += 1;
                    if size > 12 && (recv_buf[0] & 0xC0) == 0x80 {
                        let rtp_payload = &recv_buf[12..size];
                        let samples_8k = decoder.decode(rtp_payload);
                        if !samples_8k.is_empty() {
                            let resampled = simple_resample(&samples_8k, 8000, sample_rate);
                            for s in resampled { let _ = spk_prod.push(s as f32 / 32768.0); }
                        }
                    }
                }
            }
            socket.set_nonblocking(false)?;

            if last_log.elapsed().as_secs() >= 2 {
                let _ = event_tx.blocking_send(UacEvent::Log(format!("📊 RTP Stats | TX: {} | RX: {} | Target: {}", tx_count, rx_count, target)));
                last_log = std::time::Instant::now();
            }
        }
        Ok(())
    }
}