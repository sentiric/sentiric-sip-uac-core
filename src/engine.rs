// sentiric-telecom-client-sdk/src/engine.rs

use crate::{CallState, ClientCommand, UacEvent};
use crate::rtp_engine::RtpEngine;
use crate::utils::{extract_rtp_target, discover_local_ip};
use sentiric_sip_core::{parser, Header, HeaderName, Method, SipPacket};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::{mpsc, Mutex};
use std::sync::atomic::Ordering;
use serde_json::json;

// Telemetry Module
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
    observer_client: Arc<Mutex<Option<ObserverServiceClient<tonic::transport::Channel>>>>,
    headless: bool,
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
            observer_client,
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
            tracing::info!("SIP State Transition: {:?} -> {:?}", self.state, new_state);
            self.state = new_state.clone();
            let _ = self.event_tx.try_send(UacEvent::CallStateChanged(new_state));
        }
    }

    async fn log_step(&self, msg: String, level: &str, call_id: &str) {
        let _ = self.event_tx.send(UacEvent::Log(msg.clone())).await;

        let clean_msg = msg.replace("\n", "\\n").replace("\r", "");
        let json_payload = json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "level": level,
            "service": "MOBILE-SDK",
            "call_id": call_id,
            "message": clean_msg
        }).to_string();

        let req = IngestLogRequest {
            service_name: "MOBILE-SDK".into(),
            message: json_payload,
            level: level.into(),
            trace_id: call_id.into(),
            node_id: "SDK-CLIENT".into(), 
        };
        let _ = self.telemetry_tx.try_send(req);
    }

    pub async fn run(&mut self) {
        let local_ip = discover_local_ip();
        
        // 1. SIP Soketi (Tokio Async)
        let sip_socket = match TokioUdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let _ = self.event_tx.send(UacEvent::Error(format!("SIP Bind Fail: {}", e))).await;
                return;
            }
        };

        // 2. RTP Soketi (Standart Senkron - NonBlocking)
        let rtp_socket_std = match std::net::UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => {
                s.set_nonblocking(true).expect("Failed to set non-blocking on RTP socket");
                Arc::new(s)
            },
            Err(e) => {
                let _ = self.event_tx.send(UacEvent::Error(format!("RTP Bind Fail: {}", e))).await;
                return;
            }
        };

        let sip_port = sip_socket.local_addr().unwrap().port();
        let rtp_port = rtp_socket_std.local_addr().unwrap().port();

        let mut buf = [0u8; 4096];

        let mut current_target: Option<SocketAddr> = None;
        let mut current_call_id = String::new();
        let mut current_from_tag = String::new();
        let mut current_to_tag = String::new();
        let mut current_cseq = 0;

        let mut last_invite_packet: Option<Vec<u8>> = None;
        let mut retransmit_interval = tokio::time::interval(Duration::from_millis(500));
        let mut stats_ticker = tokio::time::interval(Duration::from_millis(1000));
        let mut nat_keepalive_interval = tokio::time::interval(Duration::from_secs(15));
        
        let mut invite_sent_time: Option<std::time::Instant> = None;
        let mut media_active_reported = false;

        // RTP Motoruna özel ayrılmış std soketini veriyoruz
        self.rtp_engine = Some(RtpEngine::new(rtp_socket_std.clone(), self.headless, self.event_tx.clone()));

        loop {
            tokio::select! {
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        ClientCommand::StartCall { target_ip, target_port, to_user, from_user } => {
                            if self.state != CallState::Idle { continue; }
                            
                            let observer_url = format!("http://{}:11071", target_ip);
                            if let Ok(client) = ObserverServiceClient::connect(observer_url).await {
                                let mut guard = self.observer_client.lock().await;
                                *guard = Some(client);
                            }

                            let target_addr: SocketAddr = format!("{}:{}", target_ip, target_port).parse().unwrap();
                            current_target = Some(target_addr);
                            current_call_id = format!("uac-{:x}", rand::random::<u32>());
                            current_from_tag = format!("tag-{:x}", rand::random::<u16>());
                            current_cseq = 1;

                            let mut invite = SipPacket::new_request(Method::Invite, format!("sip:{}@{}:{}", to_user, target_ip, target_port));
                            let branch = sentiric_sip_core::utils::generate_branch_id();

                            invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={};rport", local_ip, sip_port, branch)));
                            invite.headers.push(Header::new(HeaderName::From, format!("<sip:{}@sentiric.mobile>;tag={}", from_user, current_from_tag)));
                            invite.headers.push(Header::new(HeaderName::To, format!("<sip:{}@{}>", to_user, target_ip)));
                            invite.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                            invite.headers.push(Header::new(HeaderName::CSeq, format!("{} INVITE", current_cseq)));
                            invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:{}@{}:{}>", from_user, local_ip, sip_port)));
                            invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                            invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric-Telecom-SDK/4.0".to_string()));

                            let now = chrono::Utc::now().timestamp();
                            // SDP İÇİNDE RTP PORTUNU BİLDİRİYORUZ
                            let sdp = format!(
                                "v=0\r\no=- {} {} IN IP4 {}\r\ns=Sentiric Session\r\nc=IN IP4 {}\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\na=ptime:20\r\n", 
                                now, now, local_ip, local_ip, rtp_port
                            );
                            invite.body = sdp.as_bytes().to_vec();

                            let packet_bytes = invite.to_bytes();
                            self.log_step(format!("📤 SENDING INVITE:\n{}", String::from_utf8_lossy(&packet_bytes)), "INFO", &current_call_id).await;

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
                                    
                                    bye.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={};rport", local_ip, sip_port, branch)));
                                    bye.headers.push(Header::new(HeaderName::From, format!("<sip:mobile@sentiric>;tag={}", current_from_tag)));
                                    bye.headers.push(Header::new(HeaderName::To, current_to_tag.clone()));
                                    bye.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                                    
                                    current_cseq += 1;
                                    bye.headers.push(Header::new(HeaderName::CSeq, format!("{} BYE", current_cseq)));

                                    let _ = sip_socket.send_to(&bye.to_bytes(), target).await;
                                    self.log_step("📤 BYE Sent".into(), "INFO", &current_call_id).await;
                                }
                                if let Some(rtp) = &self.rtp_engine { rtp.stop(); }
                                self.change_state(CallState::Terminated);
                                self.change_state(CallState::Idle);
                                media_active_reported = false;
                            }
                        }
                    }
                },

                Ok((size, src)) = sip_socket.recv_from(&mut buf) => {
                    if size < 4 || (buf[0] & 0x80) != 0 { continue; }

                    let raw_in = String::from_utf8_lossy(&buf[..size]).to_string();
                    if self.state != CallState::Connected {
                        self.log_step(format!("📥 RECEIVED from {}:\n{}", src, raw_in), "INFO", &current_call_id).await;
                    }

                    if let Ok(packet) = parser::parse(&buf[..size]) {
                         if packet.is_response() && packet.status_code >= 100 { last_invite_packet = None; }
                         
                         if packet.is_response() && packet.status_code == 200 && (self.state == CallState::Dialing || self.state == CallState::Ringing) {
                             if let Some(to) = packet.get_header_value(HeaderName::To) { current_to_tag = to.clone(); }
                             
                             if let Some(rtp_target) = extract_rtp_target(&packet.body, &src.ip().to_string()) {
                                 self.log_step(format!("🎤 SDP Parsed. Target: {}", rtp_target), "INFO", &current_call_id).await;
                                 if let Some(rtp) = &self.rtp_engine { rtp.start(rtp_target); }
                             } else {
                                 self.log_step("⚠️ SDP Parsing Failed: No RTP target found!".into(), "WARN", &current_call_id).await;
                             }
                             
                             let mut ack = SipPacket::new_request(Method::Ack, format!("sip:{}", src));
                             let branch = sentiric_sip_core::utils::generate_branch_id();
                             ack.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={};rport", local_ip, sip_port, branch)));
                             ack.headers.push(Header::new(HeaderName::From, format!("<sip:mobile@sentiric>;tag={}", current_from_tag)));
                             ack.headers.push(Header::new(HeaderName::To, current_to_tag.clone()));
                             ack.headers.push(Header::new(HeaderName::CallId, current_call_id.clone()));
                             ack.headers.push(Header::new(HeaderName::CSeq, format!("{} ACK", current_cseq)));
                             
                             if let Some(target) = current_target {
                                 let _ = sip_socket.send_to(&ack.to_bytes(), target).await;
                                 self.log_step("--> ACK Sent".into(), "INFO", &current_call_id).await;
                             }
                             self.change_state(CallState::Connected);
                         }
                    }
                },

                _ = retransmit_interval.tick() => {
                    if let Some(packet) = &last_invite_packet {
                        if let Some(target) = current_target {
                            if let Some(start_time) = invite_sent_time {
                                if start_time.elapsed() > Duration::from_secs(5) {
                                    self.log_step("❌ Connection Failure: SBC Timeout".into(), "ERROR", &current_call_id).await;
                                    last_invite_packet = None;
                                    self.change_state(CallState::Terminated);
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
                        let rx = rtp.rx_count.load(Ordering::Relaxed);
                        let tx = rtp.tx_count.load(Ordering::Relaxed);
                        if rx > 0 || tx > 0 {
                            let _ = self.event_tx.try_send(UacEvent::RtpStats { rx_cnt: rx, tx_cnt: tx });
                            if !media_active_reported && rx > 10 {
                                media_active_reported = true;
                                self.log_step("🟢 MEDIA ACTIVE: Session flow verified.".into(), "INFO", &current_call_id).await;
                                let _ = self.event_tx.try_send(UacEvent::MediaActive);
                            }
                        }
                    }
                }
            }
        }
    }
}