//! Interface language: English (the source strings) or Indonesian. Strings stay English in the code and go
//! through `t` where they are shown; anything missing from the table shows in English.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Id,
}

static LANG: AtomicU8 = AtomicU8::new(0);

/// Config value ("auto", "en", "id") → the language to show. Auto follows the Windows display language.
pub fn resolve(setting: &str) -> Lang {
    match setting {
        "id" => Lang::Id,
        "en" => Lang::En,
        _ => {
            let ui = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
            if ui & 0x3FF == 0x21 {
                Lang::Id // LANG_INDONESIAN
            } else {
                Lang::En
            }
        }
    }
}

pub fn set(setting: &str) {
    LANG.store(resolve(setting) as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == Lang::Id as u8 {
        Lang::Id
    } else {
        Lang::En
    }
}

/// `s` in the interface language.
pub fn t(s: &str) -> &str {
    if lang() == Lang::En {
        return s;
    }
    id(s).unwrap_or(s)
}

/// A template with one `{}` (e.g. "Hide {}"), translated, then filled in.
pub fn fill(template: &str, v: &str) -> String {
    t(template).replacen("{}", v, 1)
}

fn id(s: &str) -> Option<&'static str> {
    Some(match s {
        // menu bar
        "File" => "File",
        "Edit" => "Edit",
        "View" => "Tampilan",
        "Window" => "Jendela",
        "Help" => "Bantuan",
        "Hide {}" => "Sembunyikan {}",
        "Quit {}" => "Keluar dari {}",
        "{} Help" => "Bantuan {}",
        "Update Available: MacDock {}…" => "Pembaruan Tersedia: MacDock {}…",
        "About This PC" => "Tentang PC Ini",
        "Check for MacDock Updates…" => "Periksa Pembaruan MacDock…",
        "System Settings…" => "Pengaturan Sistem…",
        "MacDock Settings…" => "Pengaturan MacDock…",
        "Force Quit…" => "Paksa Keluar…",
        "Sleep" => "Tidur",
        "Restart…" => "Mulai Ulang…",
        "Shut Down…" => "Matikan…",
        "Lock Screen" => "Kunci Layar",
        "Quit MacDock" => "Keluar dari MacDock",
        "Open MacDock at Login" => "Buka MacDock saat Login",
        "Automatically Hide Dock" => "Sembunyikan Dock Otomatis",
        "Show AI Usage" => "Tampilkan Pemakaian AI",
        "Show Battery Percentage" => "Tampilkan Persentase Baterai",
        "Show Tray Icons" => "Tampilkan Ikon Tray",
        "Hide Others" => "Sembunyikan Lainnya",
        "New Window" => "Jendela Baru",
        "New Tab" => "Tab Baru",
        "Open…" => "Buka…",
        "Open" => "Buka",
        "Save" => "Simpan",
        "Save As…" => "Simpan Sebagai…",
        "Print…" => "Cetak…",
        "Close Window" => "Tutup Jendela",
        "Undo" => "Urungkan",
        "Redo" => "Ulangi",
        "Cut" => "Potong",
        "Copy" => "Salin",
        "Paste" => "Tempel",
        "Select All" => "Pilih Semua",
        "Find…" => "Cari…",
        "Emoji & Symbols" => "Emoji & Simbol",
        "Zoom In" => "Perbesar",
        "Zoom Out" => "Perkecil",
        "Actual Size" => "Ukuran Asli",
        "Enter Full Screen" => "Masuk Layar Penuh",
        "Minimize" => "Minimalkan",
        "Zoom" => "Zoom",
        "Move to Left Half" => "Pindah ke Separuh Kiri",
        "Move to Right Half" => "Pindah ke Separuh Kanan",
        "Windows Tips" => "Tips Windows",
        // dock
        "Turn Hiding On" => "Nyalakan Sembunyi Otomatis",
        "Turn Hiding Off" => "Matikan Sembunyi Otomatis",
        "Show Recent Apps" => "Tampilkan App Terbaru",
        "Show Downloads Stack" => "Tampilkan Tumpukan Unduhan",
        "Dock Settings…" => "Pengaturan Dock…",
        "Keep in Dock" => "Simpan di Dock",
        "Remove from Dock" => "Hapus dari Dock",
        "Remove from Recents" => "Hapus dari Terbaru",
        "Change Icon…" => "Ubah Ikon…",
        "Reset Icon" => "Kembalikan Ikon",
        "Quit" => "Keluar",
        "Open in Explorer" => "Buka di Explorer",
        "Show in Explorer" => "Tampilkan di Explorer",
        "Open Downloads" => "Buka Unduhan",
        "No files" => "Tidak ada file",
        "Empty Recycle Bin" => "Kosongkan Recycle Bin",
        // launchpad
        "Search" => "Cari",
        "Loading apps…" => "Memuat app…",
        "No Results" => "Tidak Ada Hasil",
        // the Apps window and its categories
        "Applications" => "Aplikasi",
        "Apps" => "Apps",
        "Spotlight Search" => "Cari dengan Spotlight",
        "Application" => "Aplikasi",
        "Search the Web for" => "Cari di web",
        "Utilities" => "Utilitas",
        "Productivity & Finance" => "Produktivitas & Keuangan",
        "Entertainment" => "Hiburan",
        "Information & Reading" => "Informasi & Bacaan",
        "Social" => "Sosial",
        "Creativity" => "Kreativitas",
        "Developer Tools" => "Alat Developer",
        "Games" => "Game",
        "Other" => "Lainnya",
        "All apps" => "Semua app",
        "Apps window" => "Jendela Apps",
        "Full screen" => "Layar penuh",
        // settings
        "MacDock Settings" => "Pengaturan MacDock",
        "Dock" => "Dock",
        "Size" => "Ukuran",
        "Magnification" => "Pembesaran",
        "Off" => "Mati",
        "Automatically hide the dock" => "Sembunyikan dock otomatis",
        "Show recent apps" => "Tampilkan app terbaru",
        "Show Downloads stack" => "Tampilkan tumpukan Unduhan",
        "Icons on rounded tiles" => "Ikon di atas kotak bulat",
        "Notification badges" => "Lencana notifikasi",
        "Menu Bar & Desktop" => "Menu Bar & Desktop",
        "Hide the Windows taskbar" => "Sembunyikan taskbar Windows",
        "Menu bar" => "Menu bar",
        "Battery percentage" => "Persentase baterai",
        "Docks on secondary monitors" => "Dock di monitor sekunder",
        "AI usage in the menu bar" => "Pemakaian AI di menu bar",
        "Claude limits from your Claude login" => "Batas Claude dari login Claude",
        "Tray icons in the menu bar" => "Ikon tray di menu bar",
        "Desktop widgets" => "Widget desktop",
        "Mac-style Alt+Tab" => "Alt+Tab ala Mac",
        "restart" => "restart",
        "Animation" => "Animasi",
        "Genie effect when minimizing" => "Efek Genie saat minimize",
        "Minimize duration" => "Durasi minimize",
        "Magnification speed" => "Kecepatan pembesaran",
        "Dock show / hide speed" => "Kecepatan muncul dock",
        "Hide delay" => "Jeda sembunyi",
        "Reduce motion" => "Kurangi gerakan",
        "Reduce motion snaps the dock instead of animating it. Turn the Genie effect off to minimize with the Windows animation." => "Kurangi gerakan membuat dock langsung berubah tanpa animasi. Matikan efek Genie untuk memakai animasi minimize bawaan Windows.",
        "Language" => "Bahasa",
        "Auto" => "Otomatis",
        "General" => "Umum",
        "Restart MacDock to apply the menu bar, widgets and Alt+Tab changes." => "Restart MacDock untuk menerapkan perubahan menu bar, widget dan Alt+Tab.",
        "Changes apply right away and are saved to config.json." => "Perubahan langsung berlaku dan tersimpan di config.json.",
        "Restart MacDock" => "Restart MacDock",
        "Reset Animations" => "Reset Animasi",
        "Open config.json" => "Buka config.json",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_english() {
        set("id");
        assert_eq!(t("Copy"), "Salin");
        assert_eq!(t("Something new"), "Something new");
        assert_eq!(fill("Hide {}", "Chrome"), "Sembunyikan Chrome");
        set("en");
        assert_eq!(t("Copy"), "Copy");
        assert_eq!(fill("Quit {}", "Chrome"), "Quit Chrome");
    }
}
