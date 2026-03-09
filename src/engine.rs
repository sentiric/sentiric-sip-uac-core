// sentiric-telecom-client-sdk/src/engine.rs

use crate::{CallState, ClientCommand, UacEvent};
use crate::rtp_engine::RtpEngine;
use crate::utils::{extract_rtp_target, discover_local_ip};
use crate::stun::StunClient;
use sentiric_sip_core::{parser, Header, HeaderName, Method, SipPacket};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::{mpsc, Mutex};
use serde_json::json; 

pub mod observer_proto {
    tonic::include_proto!("sentiric.observer.v1");
}
use observer_proto::observer_service_client::ObserverServiceClient;
use observer_proto::IngestLogRequest;

pub struct SipEngine {
    event_tx: mpsc::Sender<UacEvent>,
    command_rx: mpsc::Receiver<ClientCommand>,
    rtp_engine: Option<RtpEngine>,
    state: CallState,
    telemetry_tx: mpsc::Sender<IngestLogRequest>,
    _observer_client: Arc<Mutex<Option<ObserverServiceClient<tonic::transport::Channel>>>>,
    headless: bool,
}

// Geriye dönük uyumluluk ve güvenlik ağı için reaktif parser
fn extract_public_addr_from_via(via: &str, fallback_ip: &str, fallback_port: u16) -> (String, u16) {
    let mut ip = fallback_ip.to_string();
    let mut port = fallback_port;
    for param in via.split(';') {
        let p_trim = param.trim();
        if p_trim.starts_with("received=") {
            ip = p_trim[9..].to_string();
        } else if p_trim.starts_with("rport=") {
            if let Ok(p) = p_trim[6..].parse::<u16>() {
                port = p;
            }
        }
    }
    (ip, port)
}

impl SipEngine {
    pub async fn new(
        event_tx: mpsc::Sender<UacEvent>,
        command_rx: mpsc::Receiver<ClientCommand>,
        headless: bool,
    ) -> Self {
        let (tel_tx, tel_rx) = mpsc::channel::<IngestLogRequest>(500);
        let observer_client = Arc::new(Mutex::new(None));
        
        Self::spawn_telemetry_worker(tel_rx, observer_client.clone());

        Self {
            event_tx,
            command_rx,
            rtp_engine: None,
            state: CallState::Idle,
            telemetry_tx: tel_tx,
            _observer_client: observer_client,
            headless,
        }
    }

    fn spawn_telemetry_worker(
        mut rx: mpsc::Receiver<IngestLogRequest>, 
        client_container: Arc<Mutex<Option<ObserverServiceClient<tonic::transport::Channel>>>>
    ) {
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let mut guard = client_container.lock().await;
                if let Some(client) = guard.as_mut() {
                    let _ = client.ingest_log(req).await;
                }
            }
        });
    }

    fn change_state(&mut self, new_state: CallState) {
        if self.state != new_state {
            tracing::info!("🔄 SIP State Transition: {:?} -> {:?}", self.state, new_state);
            self.state = new_state.clone();
            let _ = self.event_tx.try_send(UacEvent::CallStateChanged(new_state));
        }
    }

    async fn send_telemetry(&self, severity: &str, event: &str, message: &str, call_id: &str, attributes: serde_json::Value) {
        let json_payload = json!({
            "schema_v": "1.0.0",
            "ts": chrono::Utc::now().to_rfc3339(),
            "severity": severity,
            "tenant_id": "sentiric_demo",
            "resource": {
                "service.name": "mobile-uac",
                "service.version": "1.0.0",
                "service.env": "production",
                "host.name": "mobile-device"
            },
            "trace_id": if call_id.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(call_id.to_string()) },
            "span_id": serde_json::Value::Null,
            "event": event,
            "message": message,
            "attributes": attributes
        }).to_string();

        let req = IngestLogRequest { raw_json_log: json_payload };
        let _ = self.telemetry_tx.try_send(req);
    }

    pub async fn run(&mut self) {
        let local_ip = discover_local_ip();
        let _ = self.event_tx.try_send(UacEvent::Log(format!("🔍 Discovered Local IP: {}", local_ip)));
        
        // Önce 5060 (SIP ALG tetiklemek için) deniyoruz, olmazsa rastgele port
        let sip_socket = match TokioUdpSocket::bind("0.0.0.0:5060").await {
            Ok(s) => {
                let _ = self.event_tx.try_send(UacEvent::Log("🔌 SIP bound to Port 5060".to_string()));
                Arc::new(s)
            },
            Err(_) => {
                match TokioUdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => {
                        let _ = self.event_tx.try_send(UacEvent::Log("🔌 SIP bound to Ephemeral Port".to_string()));
                        Arc::new(s)
                    },
                    Err(e) => {
                        let _ = self.event_tx.send(UacEvent::Error(format!("SIP Bind Fail: {}", e))).await;
                        return;
                    }
                }
            }
        };

        let rtp_socket_std = match std::net::UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => {
                s.set_nonblocking(true).unwrap();
                Arc::new(s)
            },
            Err(e) => {
                let _ = self.event_tx.send(UacEvent::Error(format!("RTP Bind Fail: {}", e))).await;
                return;
            }
        };

        let sip_port = sip_socket.local_addr().unwrap().port();
        let rtp_port = rtp_socket_std.local_addr().unwrap().port();

        // [MİMARİ DÜZELTME]: PROAKTİF STUN KEŞFİ
        let mut public_sip_ip = local_ip.clone();
        let mut public_sip_port = sip_port;
        
        if let Some(stun_addr) = StunClient::discover_public_addr(&sip_socket, "stun.l.google.com:19302").await {
            public_sip_ip = stun_addr.ip().to_string();
            public_sip_port = stun_addr.port();
            let _ = self.event_tx.try_send(UacEvent::Log(format!("🌍 Proactive STUN Success: NAT IP is {}:{}", public_sip_ip, public_sip_port)));
        } else {
            let _ = self.event_tx.try_send(UacEvent::Log("⚠️ STUN Timeout, falling back to Local IP".to_string()));
        }

        let mut buf = [0u8; 4096];

        let mut current_target: Option<SocketAddr> = None;
        let mut current_call_id = String::new();
        let mut current_from_tag = String::new();
        let mut current_to_tag = String::new();
        let mut current_cseq = 0;

        // [KRİTİK]: Baresip uyumluluğu için aktif NAT değişkenleri
        let mut active_contact_ip = public_sip_ip.clone();
        let mut active_contact_port = public_sip_port;

        let mut last_invite_packet: Option<Vec<u8>> = None;
        let mut retransmit_interval = tokio::time::interval(Duration::from_millis(500));
        let mut stats_ticker = tokio::time::interval(Duration::from_millis(1000));
        let mut nat_keepalive_interval = tokio::time::interval(Duration::from_secs(15));
        
        let mut invite_sent_time: Option<std::time::Instant> = None;
        let mut media_active_reported = false;

        self.rtp_engine = Some(RtpEngine::new(rtp_socket_std.clone(), self.headless, self.event_tx.clone()));

        loop {
            tokio::select! {
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        ClientCommand::StartCall { target_ip, target_port, to_user, from_user } => {
                            if self.state != CallState::Idle { continue; }
                            
                            let target_addr: SocketAddr = format!("{}:{}", target_ip, target_port).parse().unwrap();
                            current_target = Some(target_addr);
                            current_call_id = format!("uac-{:x}", rand::random::<u32>());
                            current_from_tag = format!("tag-{:x}", rand::random::<u16>());
                            current_cseq = 1;

                            // --- BARESIP-LIKE AKILLI NAT (INTELLIGENT ROUTING) ---
                            // Hedef VPN veya LAN ise STUN IP'sini çöpe atıp fiziksel IP'yi kullanıyoruz.
                            active_contact_ip = public_sip_ip.clone();
                            active_contact_port = public_sip_port;

                            if let Ok(dummy) = std::net::UdpSocket::bind("0.0.0.0:0") {
                                if dummy.connect(target_addr).is_ok() {
                                    if let Ok(local_addr) = dummy.local_addr() {
                                        let lip = local_addr.ip().to_string();
                                        // 100.x = Tailscale/VPN, 10.x / 192.168.x / 172.x = LAN
                                        if target_ip.starts_with("100.") || target_ip.starts_with("10.") || target_ip.starts_with("192.168.") || target_ip.starts_with("172.") {
                                            active_contact_ip = lip;
                                            active_contact_port = sip_port; // Orijinal fiziksel port
                                            let _ = self.event_tx.try_send(UacEvent::Log(format!("🛡️ VPN/LAN Detected. STUN bypassed -> SDP set to {}", active_contact_ip)));
                                        }
                                    }
                                }
                            }

                            let mut invite = SipPacket::new_request(Method::Invite, format!("sip:{}@{}:{}", to_user, target_ip, target_port));
                            let branch = sentiric_sip_core::utils::generate_branch_id();

                            // [KRİTİK]: Tüm başlıklar active_contact_ip ile yapılandırıldı.
                            invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={};rport", active_contact_ip, active_contact_port, branch)));
                            invite.headers.push(Header::new(HeaderName::From, format!("<sip:{}@{}>;tag={}", from_user, target_ip, current_from_tag)));
                            invite.headers.push(Header::new(HeaderName::To, format!("<sip:{}@{}>", to_user, target_ip)));
                            invite.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                            invite.headers.push(Header::new(HeaderName::CSeq, format!("{} INVITE", current_cseq)));
                            invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@{}:{}>", from_user, active_contact_ip, active_contact_port)));
                            
                            invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                            invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-Mobile-UAC/3.0".to_string()));
                            invite.headers.push(Header::new(HeaderName::Allow, "INVITE, ACK, BYE, CANCEL, OPTIONS".to_string()));

                            let now = chrono::Utc::now().timestamp();
                            // Asterisk Strict Latching sorunu çözüldü.
                            let sdp = format!(
                                "v=0\r\no=- {} {} IN IP4 {}\r\ns=Sentiric Session\r\nc=IN IP4 {}\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\na=ptime:20\r\n", 
                                now, now, active_contact_ip, active_contact_ip, rtp_port
                            );
                            invite.body = sdp.as_bytes().to_vec();

                            let packet_bytes = invite.to_bytes();
                            let _ = self.event_tx.try_send(UacEvent::Log("⬆️ INVITE sent".to_string()));
                            self.send_telemetry("INFO", "SIP_PACKET_SENT", "INVITE sent", &current_call_id, json!({"sip.method": "INVITE"})).await;

                            let _ = sip_socket.send_to(&packet_bytes, target_addr).await;
                            last_invite_packet = Some(packet_bytes);
                            invite_sent_time = Some(std::time::Instant::now());
                            retransmit_interval.reset();
                            self.change_state(CallState::Dialing);
                        },
                        
                        ClientCommand::EndCall => {
                             last_invite_packet = None;
                             if self.state != CallState::Idle {
                                if let Some(target) = current_target {
                                    let mut bye = SipPacket::new_request(Method::Bye, format!("sip:{}", target));
                                    let branch = sentiric_sip_core::utils::generate_branch_id();
                                    
                                    bye.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={};rport", active_contact_ip, active_contact_port, branch)));
                                    bye.headers.push(Header::new(HeaderName::From, format!("<sip:mobile@sentiric>;tag={}", current_from_tag)));
                                    bye.headers.push(Header::new(HeaderName::To, current_to_tag.clone()));
                                    bye.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                                    
                                    current_cseq += 1;
                                    bye.headers.push(Header::new(HeaderName::CSeq, format!("{} BYE", current_cseq)));
                                    bye.headers.push(Header::new(HeaderName::Contact, format!("<sip:mobile@{}:{}>", active_contact_ip, active_contact_port)));

                                    let packet_bytes = bye.to_bytes();
                                    let _ = self.event_tx.try_send(UacEvent::Log("⬆️ BYE sent".to_string()));
                                    let _ = sip_socket.send_to(&packet_bytes, target).await;
                                }
                                if let Some(rtp) = &self.rtp_engine { rtp.stop(); }
                                self.change_state(CallState::Terminated);
                                self.change_state(CallState::Idle);
                                media_active_reported = false;
                            }
                        },

                        ClientCommand::UpdateSettings { mic_gain, speaker_gain, enable_aec: _ } => {
                            if let Some(rtp) = &self.rtp_engine {
                                rtp.update_gains(mic_gain, speaker_gain);
                            }
                        },

                        ClientCommand::SendDtmf { key } => {
                            if let Some(rtp) = &self.rtp_engine {
                                rtp.trigger_dtmf(key);
                                let _ = self.event_tx.try_send(UacEvent::Log(format!("🎹 In-Band DTMF Injecting: {}", key)));
                            }
                        },      
                                                // YENİ: Mute Komutu Yakalayıcı
                        ClientCommand::SetMute { muted } => {
                            if let Some(rtp) = &self.rtp_engine {
                                rtp.set_mute(muted);
                                let state_str = if muted { "MUTED" } else { "UNMUTED" };
                                let _ = self.event_tx.try_send(UacEvent::Log(format!("🔇 Mic state changed: {}", state_str)));
                            }
                        }                        
                    }
                },

                Ok((size, src)) = sip_socket.recv_from(&mut buf) => {
                    if size < 4 || (buf[0] & 0x80) != 0 { continue; }

                    let raw_in = String::from_utf8_lossy(&buf[..size]).to_string();
                    let first_line = raw_in.lines().next().unwrap_or("UNKNOWN").to_string();
                    let _ = self.event_tx.try_send(UacEvent::Log(format!("⬇️ {}", first_line)));

                    if let Ok(packet) = parser::parse(&buf[..size]) {
                         
                         // 1. SUNUCUDAN GELEN ISTEKLERI (REQUESTS) YAKALA
                         if packet.is_request() {
                             if packet.method == Method::Bye {
                                 let _ = self.event_tx.try_send(UacEvent::Log("🏁 Server closed connection (BYE). Hanging up!".to_string()));
                                 self.send_telemetry("INFO", "REMOTE_HANGUP", "Received BYE from Server", &current_call_id, json!({})).await;
                                 
                                 let mut ok_resp = SipPacket::create_response_for(&packet, 200, "OK".to_string());
                                 ok_resp.headers.push(Header::new(HeaderName::Contact, format!("<sip:mobile@{}:{}>", active_contact_ip, active_contact_port)));
                                 ok_resp.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-Mobile-UAC/3.0".to_string()));
                                 
                                 let _ = sip_socket.send_to(&ok_resp.to_bytes(), src).await;
                                 let _ = self.event_tx.try_send(UacEvent::Log("⬆️ 200 OK (For BYE) sent".to_string()));

                                 if let Some(rtp) = &self.rtp_engine { rtp.stop(); }
                                 
                                 self.change_state(CallState::Terminated);
                                 self.change_state(CallState::Idle);
                                 media_active_reported = false;
                                 last_invite_packet = None;
                             }
                             continue; 
                         }

                         // 2. SUNUCUDAN GELEN CEVAPLAR (RESPONSES)
                         let status_code = packet.status_code;
                         
                         // [GÜVENLİK AĞI]: Reactive NAT Update - Asterisk ne gördüyse onu kullan
                         if status_code >= 100 && self.state == CallState::Dialing {
                             last_invite_packet = None;
                             if let Some(via) = packet.get_header_value(HeaderName::Via) {
                                 let (ip, port) = extract_public_addr_from_via(via, &active_contact_ip, active_contact_port);
                                 if ip != active_contact_ip || port != active_contact_port {
                                     let _ = self.event_tx.try_send(UacEvent::Log(format!("🌍 Reactive NAT Update: {}:{}", ip, port)));
                                     active_contact_ip = ip;
                                     active_contact_port = port;
                                 }
                             }
                         }
                         
                         if status_code == 180 || status_code == 183 {
                             self.change_state(CallState::Ringing);
                         } 
                         
                         if status_code == 200 && (self.state == CallState::Dialing || self.state == CallState::Ringing) {
                             if let Some(to) = packet.get_header_value(HeaderName::To) { current_to_tag = to.clone(); }
                             
                             if let Some(rtp_target) = extract_rtp_target(&packet.body, &src.ip().to_string()) {
                                 self.send_telemetry("INFO", "SDP_PARSED", "Media target locked", &current_call_id, json!({"target": rtp_target.to_string()})).await;
                                 if let Some(rtp) = &self.rtp_engine { rtp.start(rtp_target); }
                             }
                             
                             let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", src));
                             let branch = sentiric_sip_core::utils::generate_branch_id();

                             ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={};rport", active_contact_ip, active_contact_port, branch)));
                             if let Some(from) = packet.get_header_value(HeaderName::From) { 
                                 ack.headers.push(Header::new(HeaderName::From, from.clone())); 
                             }
                             ack.headers.push(Header::new(HeaderName::To, current_to_tag.clone()));
                             ack.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                             ack.headers.push(Header::new(HeaderName::CSeq, format!("{} ACK", current_cseq)));
                             ack.headers.push(Header::new(HeaderName::Contact, format!("<sip:mobile@{}:{}>", active_contact_ip, active_contact_port)));
                             ack.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-Mobile-UAC/3.0".to_string()));
                             
                             if let Some(target) = current_target {
                                 let ack_bytes = ack.to_bytes();
                                 let _ = self.event_tx.try_send(UacEvent::Log("⬆️ ACK sent".to_string()));
                                 let _ = sip_socket.send_to(&ack_bytes, target).await;
                             }
                             self.change_state(CallState::Connected);
                         } else if status_code >= 400 {
                             let _ = self.event_tx.try_send(UacEvent::Log(format!("❌ Server rejected with {}", status_code)));
                             self.change_state(CallState::Terminated);
                             self.change_state(CallState::Idle);
                             last_invite_packet = None;
                         }
                    }
                },

                _ = retransmit_interval.tick() => {
                    if let Some(packet) = &last_invite_packet {
                        if let Some(target) = current_target {
                            if let Some(start_time) = invite_sent_time {
                                if start_time.elapsed() > Duration::from_secs(5) {
                                    let _ = self.event_tx.try_send(UacEvent::Log("⏱️ Connection Timeout!".to_string()));
                                    last_invite_packet = None;
                                    self.change_state(CallState::Terminated);
                                    self.change_state(CallState::Idle);
                                } else {
                                    let _ = sip_socket.send_to(packet, target).await;
                                }
                            }
                        }
                    }
                },

                _ = nat_keepalive_interval.tick() => {
                    if self.state != CallState::Idle {
                        if let Some(target) = current_target {
                            let keepalive = b"\r\n\r\n";
                            let _ = sip_socket.send_to(keepalive, target).await;
                        }
                    }
                },

                _ = stats_ticker.tick() => {
                    if let Some(rtp) = &self.rtp_engine {
                        let rx = std::sync::atomic::AtomicU64::load(&rtp.rx_count, std::sync::atomic::Ordering::Relaxed);
                        let tx = std::sync::atomic::AtomicU64::load(&rtp.tx_count, std::sync::atomic::Ordering::Relaxed);
                        if rx > 0 || tx > 0 {
                            let _ = self.event_tx.try_send(UacEvent::RtpStats { rx_cnt: rx, tx_cnt: tx });
                            if !media_active_reported && rx > 10 {
                                media_active_reported = true;
                                let _ = self.event_tx.try_send(UacEvent::MediaActive);
                            }
                        }
                    }
                }
            }
        }
    }
}