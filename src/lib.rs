// sentiric-telecom-client-sdk/src/lib.rs

pub mod engine;
pub mod rtp_engine;
pub mod utils;

use tokio::sync::mpsc;

/// UI tarafına (Flutter/CLI) gönderilecek olaylar.
#[derive(Debug, Clone)]
pub enum UacEvent {
    /// SDK içi log mesajları.
    Log(String),
    /// SIP sinyalleşme durum değişiklikleri.
    CallStateChanged(CallState),
    /// İlk ses paketleri ulaştığında fırlatılır.
    MediaActive,
    /// Saniyelik ağ istatistikleri.
    RtpStats { rx_cnt: u64, tx_cnt: u64 },
    /// Kritik hatalar.
    Error(String),
}

/// SIP Çağrı Durum Makinesi (State Machine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallState {
    Idle,
    Dialing,
    Ringing,
    Connected,
    Terminated,
}

/// SDK Komutları.
pub enum ClientCommand {
    StartCall {
        target_ip: String,
        target_port: u16,
        to_user: String,
        from_user: String,
    },
    EndCall,
}

/// Sentiric Telecom Client'ın ana kapısı.
pub struct TelecomClient {
    command_tx: mpsc::Sender<ClientCommand>,
}

impl TelecomClient {
    /// Yeni bir istemci oluşturur ve motoru (Engine) arka planda başlatır.
    /// headless: true ise ses kartı yerine sanal DSP kullanılır.
    pub fn new(event_tx: mpsc::Sender<UacEvent>, headless: bool) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        
        // SipEngine'i başlat
        tokio::spawn(async move {
            let mut engine = engine::SipEngine::new(event_tx, cmd_rx, headless).await;
            engine.run().await;
        });

        Self {
            command_tx: cmd_tx,
        }
    }

    /// Yeni bir SIP araması başlatır.
    pub async fn start_call(&self, target_ip: String, target_port: u16, to_user: String, from_user: String) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::StartCall { 
            target_ip, target_port, to_user, from_user 
        }).await.map_err(|_| anyhow::anyhow!("Engine task is unreachable"))?;
        Ok(())
    }

    /// Mevcut aramayı sonlandırır (BYE gönderir).
    pub async fn end_call(&self) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::EndCall).await
            .map_err(|_| anyhow::anyhow!("Engine task is unreachable"))?;
        Ok(())
    }
}