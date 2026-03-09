// sentiric-telecom-client-sdk/src/lib.rs

pub mod engine;
pub mod rtp_engine;
pub mod utils;
pub mod stun;
pub mod media; // YENİ: Medya Soyutlama Katmanı

use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum UacEvent {
    Log(String),
    CallStateChanged(CallState),
    MediaActive,
    RtpStats { rx_cnt: u64, tx_cnt: u64 },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallState {
    Idle,
    Dialing,
    Ringing,
    Connected,
    Terminated,
}

pub enum ClientCommand {
    StartCall {
        target_ip: String,
        target_port: u16,
        to_user: String,
        from_user: String,
    },
    EndCall,
    UpdateSettings {
        mic_gain: f32,
        speaker_gain: f32,
        enable_aec: bool,
    },
    SendDtmf {
        key: char,
    },
    SetMute {
        muted: bool,
    },
}

pub struct TelecomClient {
    command_tx: mpsc::Sender<ClientCommand>,
}

impl TelecomClient {
    pub fn new(event_tx: mpsc::Sender<UacEvent>, headless: bool) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        
        tokio::spawn(async move {
            let mut engine = engine::SipEngine::new(event_tx, cmd_rx, headless).await;
            engine.run().await;
        });

        Self { command_tx: cmd_tx }
    }

    pub async fn start_call(&self, target_ip: String, target_port: u16, to_user: String, from_user: String) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::StartCall { target_ip, target_port, to_user, from_user }).await.map_err(|_| anyhow::anyhow!("Engine task is unreachable"))?;
        Ok(())
    }

    pub async fn end_call(&self) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::EndCall).await.map_err(|_| anyhow::anyhow!("Engine task is unreachable"))?;
        Ok(())
    }
    
    // YENİ: Mute API
    pub async fn set_mute(&self, muted: bool) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::SetMute { muted }).await.map_err(|_| anyhow::anyhow!("Engine task is unreachable"))?;
        Ok(())
    }

    // YENİ: Otomasyon botlarının DTMF basabilmesi için dışa açılan metod
    pub async fn send_dtmf(&self, key: char) -> anyhow::Result<()> {
        self.command_tx.send(ClientCommand::SendDtmf { key }).await
            .map_err(|_| anyhow::anyhow!("Engine task is unreachable"))?;
        Ok(())
    }
}