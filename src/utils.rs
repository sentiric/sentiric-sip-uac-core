// sentiric-telecom-client-sdk/src/utils.rs

use std::net::{SocketAddr, UdpSocket};

/// SDP içinden c= ve m= satırlarını okuyarak gerçek RTP hedefini bulur.
pub fn extract_rtp_target(sdp_body: &[u8], default_ip: &str) -> Option<SocketAddr> {
    let sdp_str = String::from_utf8_lossy(sdp_body);
    let mut ip = default_ip.to_string();
    let mut port = 0u16;

    for line in sdp_str.lines() {
        if line.starts_with("c=IN IP4 ") {
            let parsed_ip = line[9..].trim();
            if parsed_ip != "0.0.0.0" {
                ip = parsed_ip.to_string();
            }
        } else if line.starts_with("m=audio ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() > 1 {
                port = parts[1].parse().unwrap_or(0);
            }
        }
    }

    if port > 0 {
        if let Ok(addr) = format!("{}:{}", ip, port).parse() {
            return Some(addr);
        }
    }
    None
}

/// [NEW ADIM 3]: Cihazın o anki aktif ağ arayüzü IP'sini keşfeder.
/// 0.0.0.0 kullanımını bitiren zeki mekanizma.
pub fn discover_local_ip() -> String {
    // Rastgele bir dış adrese "bağlanıyormuş" gibi yapıyoruz. 
    // Bu işlem internet gerektirmez, sadece OS yönlendirme tablosuna bakar.
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return "127.0.0.1".to_string(),
    };

    if socket.connect("8.8.8.8:80").is_ok() {
        if let Ok(local_addr) = socket.local_addr() {
            return local_addr.ip().to_string();
        }
    }

    // Fallback: Eğer ağ yoksa veya kısıtlıysa loopback
    "127.0.0.1".to_string()
}