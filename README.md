# Sentiric SIP UAC Core

Bu kütüphane, Sentiric ekosistemi için **SIP User Agent Client (UAC)** mantığını barındıran temel Rust motorudur. Hem CLI (Terminal) hem de Mobil (Flutter) uygulamalar tarafından ortak çekirdek olarak kullanılır.

## 🎯 Özellikler

*   **Platform Bağımsız:** UI içermez, durumları `Event Stream` üzerinden bildirir.
*   **Tam Yetenekli:** SIP Register, Invite, Ack, Bye ve RTP akışını yönetir.
*   **Flutter Uyumlu:** `flutter_rust_bridge` ile mobil cihazlarda çalışmaya hazırdır.

## 📦 Kullanım

```toml
[dependencies]
sentiric-sip-uac-core = { git = "https://github.com/sentiric/sentiric-sip-uac-core.git" }
```