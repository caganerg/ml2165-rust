//! SIGTERM ile iş iptali.
//!
//! Kullanıcı bir işi iptal ettiğinde `cupsd`, filtre zincirindeki her sürece
//! SIGTERM gönderir. Varsayılan eylem süreci ANINDA sonlandırmaktır: `Drop`
//! çalışmaz, dolayısıyla `SplStreamWriter`'ın kapanış UEL'i güvenlik ağı da
//! (bkz. spl.rs `impl Drop for SplStreamWriter`) en sık görülen yarıda kalma
//! senaryosunda devre dışı kalırdı. Yazıcı, `@PJL ENTER LANGUAGE = QPDL`
//! sonrasında yarım bir sayfa ile bırakılırdı.
//!
//! CUPS'ın kendi tasarımı bunu filtreden bekler: USB backend'i stdin'den
//! okurken SIGTERM'i KASITLI olarak yok sayar (backend/usb-*.c: "ignore
//! SIGTERM so that the driver can finish out any page data") ki sürücü işi
//! düzgün kapatabilsin. CUPS'ın `rastertoepson`/`rastertolabel` filtreleri de
//! bu yüzden SIGTERM'i yakalar.
//!
//! İşleyici yalnızca bir bayrak kaldırır (async-signal-safe tek işlem budur);
//! filtre bayrağı her sayfa ve her bant başında `check` ile okur ve iptali
//! sıradan bir hata gibi yukarı döndürür. Böylece iptal, kesik veri gibi diğer
//! hatalarla AYNI kapanış yolunu kullanır: writer düşer, `Drop` kapanış UEL'ini
//! yazar.

use std::ffi::c_int;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// Linux'ta (paketin hedefi olan amd64 dahil tüm mimarilerde) SIGTERM = 15.
const SIGTERM: c_int = 15;

/// `signal()`'ın hata dönüşü: `(sighandler_t) -1`.
const SIG_ERR: usize = usize::MAX;

unsafe extern "C" {
    /// glibc'nin `signal()`'ı BSD anlamını taşır: işleyici kalıcıdır ve
    /// `SA_RESTART` açıktır, yani sinyal gelince bloklu bir `read()`/`write()`
    /// EINTR ile kesilmez, kaldığı yerden sürer. İstenen de budur: iptal,
    /// bir sonraki bant sınırında `check` ile ele alınır. Önceki filtre de aynı
    /// SIGTERM'i aldığı için bloklu bir okuma en geç onun çıkışıyla (EOF) biter.
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
}

/// `main`'in kullandığı süreç geneli iptal bayrağı. Testler bunun yerine
/// kendi yerel `AtomicBool`'larını geçirir ki paralel testler birbirini
/// iptal etmesin.
pub static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_signum: c_int) {
    REQUESTED.store(true, Ordering::Relaxed);
}

/// SIGTERM işleyicisini kurar.
pub fn install_sigterm_handler() -> io::Result<()> {
    // SAFETY: `on_sigterm` yalnızca bir atomik değişkene yazar; bu,
    // sinyal işleyicisinde güvenle yapılabilecek bir işlemdir.
    if unsafe { signal(SIGTERM, on_sigterm) } == SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// İptal istenmişse hata döndürür; sayfa ve bant döngülerinin başında çağrılır.
pub fn check(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(io::Error::other("İş iptal edildi (SIGTERM)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn raise(sig: c_int) -> c_int;
    }

    /// Kurulu işleyici SIGTERM'i süreci öldürmek yerine bayrağa çevirmeli.
    /// İşleyici kurulamazsa bu test, test sürecinin kendisini sonlandırarak
    /// (dolayısıyla gürültülü biçimde) başarısız olur.
    ///
    /// Süreç geneli `REQUESTED`'ı yalnızca bu test kullanır; diğer testler
    /// yerel bayrak geçirdiği için paralel çalışmaları etkilenmez.
    #[test]
    fn test_sigterm_sets_flag_instead_of_terminating() {
        install_sigterm_handler().expect("SIGTERM işleyicisi kurulamadı");
        // `raise`, işleyici döndükten SONRA döner.
        assert_eq!(unsafe { raise(SIGTERM) }, 0);
        assert!(REQUESTED.load(Ordering::Relaxed));
        assert!(check(&REQUESTED).is_err());
    }

    #[test]
    fn test_check_passes_when_not_cancelled() {
        assert!(check(&AtomicBool::new(false)).is_ok());
    }
}
