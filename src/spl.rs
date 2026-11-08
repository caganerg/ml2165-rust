//! # Samsung Printer Language (SPL2 / QPDL v3) Protokol Modülü
//!
//! Samsung ML-2165 için PJL iş zarfı, 17 baytlık QPDL sayfa başlığı ve
//! 0x09ABCDEF alt başlıklı, sağlama toplamlı Algo 0x11 RLE şerit kayıtları.
//!
//! Kaynak: OpenPrinting SpliX (`printer.cpp`, `qpdl.cpp`, `algo0x11.cpp`).
//! Lisans: GPLv2 (yalnızca v2 — SpliX kaynağıyla aynı)

use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// PJL Universal Exit Language (UEL)
pub const PJL_UEL: &[u8] = b"\x1b%-12345X";
pub const PJL_END: &[u8] = b"\t\x1b%-12345X";

/// Alt Başlık İmzası (Sub-header Signature: 0x09ABCDEF - Little Endian)
pub const SUBHEADER_SIG_LE: [u8; 4] = [0xEF, 0xCD, 0xAB, 0x09];

/// Algo 0x11 RLE Sıkıştırma Sabitleri
pub const COMPRESS_SAMPLE_RATE: usize = 0x800; // 2048 bayt
pub const TABLE_PTR_SIZE: usize = 0x40; // 64 adet işaretçi/ofset
pub const MAX_UNCOMPRESSED_BYTES: usize = 0x80; // 128 bayt
pub const MAX_COMPRESSED_BYTES: usize = 0x1FF + 3; // 514 bayt
pub const COMPRESSION_FLAG: u8 = 0x80;

/// Şerit kaydındaki sıkıştırma kodu. SpliX bu yazıcı ailesine hiçbir zaman
/// ham (0x00) bant göndermez; firmware ham bandı "INTERNAL ERROR - Please use
/// the proper driver" ile reddeder. Bu filtre de her zaman Algo 0x11 kullanır.
const COMPRESSION_ALGO_0X11: u8 = 0x11;

/// QPDL sayfa başlığındaki protokol sürümü (`header[0xE]`).
const QPDL_VERSION: u8 = 3;

/// QPDL kağıt boyutu kodları; yalnızca PPD'nin sunduğu ölçüler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SplPaperSize {
    Letter = 0,
    Legal = 1,
    A4 = 2,
    Executive = 3,
    Env10 = 6,
    C5 = 8,
    Dl = 9,
    B5 = 11,
    A5 = 16,
    A6 = 17,
    Folio = 24,
}

impl SplPaperSize {
    /// Fiziksel ölçüyü (pt, iki yönde de) bir QPDL kağıt koduna eşler.
    ///
    /// Tablo dışındaki ölçüler başka bir koda (ör. A4'e) düşürülmez, `None`
    /// döner: sayfa başlığındaki kağıt kodu ile gönderilen raster geometrisi
    /// ayrışırsa yazıcı yanlış hizalar ya da yanlış tepsiden besler.
    pub fn from_dimensions_pt_exact(width_pt: u32, height_pt: u32) -> Option<Self> {
        Some(match (width_pt, height_pt) {
            (595, 842) | (842, 595) => SplPaperSize::A4,
            (612, 792) | (792, 612) => SplPaperSize::Letter,
            (612, 1008) | (1008, 612) => SplPaperSize::Legal,
            (420, 595) | (595, 420) => SplPaperSize::A5,
            (297, 420) | (420, 297) => SplPaperSize::A6,
            (522, 756) | (756, 522) => SplPaperSize::Executive,
            // Folio (F4) = 210x330 mm. 612x936 (8.5x13 inç) Folio değil,
            // FanFoldGermanLegal'dir; upstream SpliX'in ml2165.ppd'si de 595x935 der.
            (595, 935) | (935, 595) => SplPaperSize::Folio,
            (516, 729) | (729, 516) => SplPaperSize::B5,
            (297, 684) | (684, 297) => SplPaperSize::Env10,
            (312, 624) | (624, 312) => SplPaperSize::Dl,
            (459, 649) | (649, 459) => SplPaperSize::C5,
            _ => return None,
        })
    }
}

/// QPDL kağıt kaynağı kodları (SpliX printer.cpp). ML-2165'te yalnızca kaset
/// ve elle besleme yuvası vardır.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SplPaperSource {
    Auto = 1,
    Manual = 2,
}

impl SplPaperSource {
    /// CUPS Raster başlığındaki `MediaPosition` alanını QPDL koduna eşler.
    ///
    /// PPD `*InputSlot` seçeneklerini doğrudan QPDL kodlarıyla numaralandırır
    /// (`<</MediaPosition 1>>` = Auto, `2` = Manual); bağı
    /// `test_every_ppd_input_slot_maps_to_its_qpdl_code` denetler. `0`, PPD'siz
    /// üretilmiş bir raster'da "seçilmedi" demektir ve Auto'dur. Diğer değerler
    /// `None` döner ki çağıran uyarabilsin.
    pub fn from_media_position(media_position: u32) -> Option<Self> {
        match media_position {
            0 | 1 => Some(SplPaperSource::Auto),
            2 => Some(SplPaperSource::Manual),
            _ => None,
        }
    }
}

/// Eksen başına QPDL çözünürlüğü.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplResolution {
    Dpi300,
    Dpi600,
    Dpi1200,
}

impl SplResolution {
    #[inline]
    pub fn dpi(&self) -> u32 {
        match self {
            SplResolution::Dpi300 => 300,
            SplResolution::Dpi600 => 600,
            SplResolution::Dpi1200 => 1200,
        }
    }

    /// Yalnızca tam 300/600/1200 değerlerini dönüştürür; yakındaki değere
    /// yuvarlamak, raster geometrisiyle uyuşmayan bir DPI bildirmek olurdu.
    pub fn from_dpi_exact(dpi: u32) -> Option<Self> {
        match dpi {
            300 => Some(SplResolution::Dpi300),
            600 => Some(SplResolution::Dpi600),
            1200 => Some(SplResolution::Dpi1200),
            _ => None,
        }
    }

    /// PPD'nin sunduğu çözünürlük çiftleri. Eksenleri ayrı ayrı doğrulamak
    /// yetmez: 600x1200 iki tanınan eksenden oluşur ama bir yazıcı modu değildir.
    pub fn pair_is_supported(x_dpi: u32, y_dpi: u32) -> bool {
        matches!(
            (x_dpi, y_dpi),
            (300, 300) | (600, 600) | (1200, 600) | (1200, 1200)
        )
    }
}

/// Yazıcının `@PJL SET PAPERTYPE` için tanıdığı kelime dağarcığı.
///
/// Kaynak, upstream SpliX'in aynı motor ailesine ait PPD'lerindeki
/// `*MediaType` anahtarlarıdır (ml1910, ml2010, ml2525, ml1640, ml2510).
/// SpliX anahtarı olduğu gibi `@PJL SET PAPERTYPE=%s` satırına yazar; yani
/// anahtarın kendisi protokol değeridir. `OFF` = yazıcının kendi varsayılanı.
pub const PJL_PAPER_TYPES: [&str; 14] = [
    "OFF", "NORMAL", "THICK", "THIN", "BOND", "OHP", "CARD", "LABEL", "USED", "COLOR", "ENV",
    "COTTON", "RECYCLED", "ARCHIVE",
];

/// Kağıt türü bilinmediğinde ya da tanınmadığında kullanılan PJL değeri.
pub const PJL_PAPERTYPE_DEFAULT: &str = PJL_PAPER_TYPES[0];

/// Raster başlığındaki `MediaType` metnini yazıcının PJL değerine eşler.
///
/// Değer işi gönderen istemciden gelir ve güvenilmezdir. `PAPERTYPE` satırı
/// tırnaksız olduğu için tek bir boşluk bile satırı bozardı; bu yüzden metin
/// satıra hiç yazılmaz, yalnızca tabloya eşlenir ve dönen `&'static str`
/// her zaman yukarıdaki sabitlerden biridir. Karşılaştırma ASCII büyük/küçük
/// harfe duyarsızdır ve baştaki/sondaki boşlukları yok sayar.
pub fn pjl_paper_type(name: &str) -> Option<&'static str> {
    let name = name.trim();
    PJL_PAPER_TYPES
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(name))
        .copied()
}

/// Unix çağından itibaren geçen gün sayısını `YYYYMMDD` biçimine çevirir
/// (Howard Hinnant'ın `civil_from_days` algoritması). Tarih/saat crate'i
/// gerektirmemek için elle yazıldı.
fn service_date_from_unix_days(days_since_epoch: i64) -> String {
    let z = days_since_epoch + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };

    format!("{year:04}{month:02}{day:02}")
}

/// Geçerli UTC tarihini PJL `SERVICEDATE` biçiminde (`YYYYMMDD`) döndürür.
/// Sistem saati Unix çağından önceyse 1970-01-01 kullanılır.
pub fn current_service_date() -> String {
    let days_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_secs() / 86_400) as i64)
        .unwrap_or(0);
    service_date_from_unix_days(days_since_epoch)
}

/// İş Konfigürasyonu
#[derive(Debug, Clone)]
pub struct JobConfig {
    pub job_name: String,
    pub user_name: String,
    pub service_date: String,
    /// `@PJL SET PAPERTYPE` değeri; yalnızca `PJL_PAPER_TYPES` sabitlerinden
    /// biri olabilir (bkz. `pjl_paper_type`).
    pub paper_type: &'static str,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            job_name: "CUPS Document".to_string(),
            user_name: "guest".to_string(),
            service_date: current_service_date(),
            paper_type: PJL_PAPERTYPE_DEFAULT,
        }
    }
}

/// Sayfa Konfigürasyonu
#[derive(Debug, Clone)]
pub struct PageConfig {
    pub paper_size: SplPaperSize,
    pub paper_source: SplPaperSource,
    /// Yatay çözünürlük — sayfa başlığında `header[0x10]`.
    ///
    /// Eksenler ayrı alanlardır ve sıraları sezgiye aykırıdır: SpliX qpdl.cpp
    /// `header[0x1] = yResolution / 100`, `header[0x10] = xResolution / 100`.
    /// `1200x600dpi` modunda ikisini karıştırmak sayfayı dikeyde iki kat ezer.
    pub resolution_x: SplResolution,
    /// Dikey çözünürlük — sayfa başlığında `header[0x1]`.
    pub resolution_y: SplResolution,
    pub copies: u16,
    /// Sayfa başlığındaki genişlik/yükseklik alanları 16 bittir. Tipin de
    /// `u16` olması, çağıranı daraltmayı (`try_from`) açıkça ele almaya zorlar.
    pub width_pixels: u16,
    pub height_pixels: u16,
}

impl Default for PageConfig {
    /// A4 @ 600 DPI, tek kopya, kaset.
    fn default() -> Self {
        Self {
            paper_size: SplPaperSize::A4,
            paper_source: SplPaperSource::Auto,
            resolution_x: SplResolution::Dpi600,
            resolution_y: SplResolution::Dpi600,
            copies: 1,
            width_pixels: 4960,
            height_pixels: 6817,
        }
    }
}

// ============================================================================
// Algo 0x11 RLE Sıkıştırıcı (SpliX algo0x11.cpp)
// ============================================================================

pub struct Algo0x11;

impl Algo0x11 {
    /// Veride en sık tekrar eden 64 geri ofseti seçer; sıkıştırıcı eşleşmeleri
    /// yalnızca bu tablodaki ofsetlerde arar ve tablo payload'la birlikte gider.
    pub fn lookup_best_offsets(data: &[u8]) -> [u16; TABLE_PTR_SIZE] {
        // 2048 x (u32, usize) = 32 KB; yığıt yerine öbekte tutuluyor.
        let mut occurrences: Vec<(u32, usize)> =
            (0..COMPRESS_SAMPLE_RATE).map(|i| (0u32, i)).collect();

        // Veri örnekleme aralığından uzunsa her 2048 baytta bir örnek alınır;
        // kısaysa her bayt taranır.
        let (start, step) = if data.len() >= COMPRESS_SAMPLE_RATE {
            (COMPRESS_SAMPLE_RATE, COMPRESS_SAMPLE_RATE)
        } else {
            (1, 1)
        };

        let mut i = start;
        while i < data.len() {
            let b = data[i];
            let max_j = COMPRESS_SAMPLE_RATE.min(i);
            for j in 1..max_j {
                if data[i - j] == b {
                    occurrences[j - 1].0 += 1;
                }
            }
            i += step;
        }

        occurrences.sort_unstable_by_key(|&(count, _)| std::cmp::Reverse(count));

        // En sık rastlanan 64 ofset. Tabloda 1 (bir önceki bayt) mutlaka
        // bulunmalı; yoksa son sıra ona ayrılır.
        let mut table = [1u16; TABLE_PTR_SIZE];
        for (slot, &(_, offset)) in table.iter_mut().zip(occurrences.iter()) {
            *slot = (offset + 1) as u16;
        }
        if !table.contains(&1) {
            table[TABLE_PTR_SIZE - 1] = 1;
        }

        table
    }

    /// Bandı Algo 0x11 ile sıkıştırır. Boş girdi için `None` döner: payload'u
    /// boş bir "sıkıştırılmış" bant kaydı yazıcı tarafında çözme hatasıdır.
    pub fn compress(data: &[u8]) -> Option<Vec<u8>> {
        if data.is_empty() {
            return None;
        }

        let ptr_array = Self::lookup_best_offsets(data);
        let mut out = Vec::with_capacity(data.len() + 256);

        // 1. uncompressed_initial_size için 4 bayt yer aç
        out.extend_from_slice(&[0u8; 4]);

        // 2. 64 adet u16 ofset tablosu
        let mut max_offset: usize = 0;
        for &ptr in &ptr_array {
            out.extend_from_slice(&ptr.to_le_bytes());
            max_offset = max_offset.max(ptr as usize);
        }

        // 3. İlk ham baytlar. Tabloda her zaman 1 bulunduğu ve veri boş
        // olmadığı için `uncomp_size` en az 1'dir.
        let uncomp_size = max_offset.min(MAX_UNCOMPRESSED_BYTES).min(data.len());
        out[0..4].copy_from_slice(&(uncomp_size as u32).to_le_bytes());
        out.extend_from_slice(&data[..uncomp_size]);

        let mut r = uncomp_size;
        let mut raw_data_counter: usize = 0;
        let mut raw_data_counter_ptr: usize = 0;

        while r < data.len() {
            let max_comp_size = (data.len() - r).min(MAX_COMPRESSED_BYTES);

            if max_comp_size >= 3 {
                let mut best_comp_counter: usize = 0;
                let mut best_ptr: usize = 0;

                for (i, &offset) in ptr_array.iter().enumerate() {
                    let off = offset as usize;
                    if off > r {
                        continue;
                    }
                    let r_ref = r - off;

                    // Klasik LZ kısayolu: `best_comp_counter`ıncı bayt
                    // tutmuyorsa bu aday mevcut en iyiyi geçemez. Seçilen
                    // eşleşmeyi değiştirmez, yalnızca karşılaştırma sayısını
                    // düşürür. İndisler sınır içinde: döngüde
                    // `best_comp_counter < max_comp_size <= data.len() - r`.
                    if data[r + best_comp_counter] != data[r_ref + best_comp_counter] {
                        continue;
                    }

                    let mut counter = 0;
                    while counter < max_comp_size && data[r + counter] == data[r_ref + counter] {
                        counter += 1;
                    }
                    if counter > best_comp_counter {
                        best_comp_counter = counter;
                        best_ptr = i;
                        if counter == max_comp_size {
                            break;
                        }
                    }
                }

                if best_comp_counter >= 3 {
                    if raw_data_counter > 0 {
                        out[raw_data_counter_ptr] = (raw_data_counter - 1) as u8;
                        raw_data_counter = 0;
                    }

                    r += best_comp_counter;
                    let comp_len = (best_comp_counter - 3) as u16;

                    let byte0 = COMPRESSION_FLAG | ((comp_len & 0x7F) as u8);
                    let byte1 = (((comp_len >> 1) & 0xC0) as u8) | ((best_ptr & 0x3F) as u8);

                    out.push(byte0);
                    out.push(byte1);
                    continue;
                }
            }

            raw_data_counter += 1;
            if raw_data_counter == 1 {
                raw_data_counter_ptr = out.len();
                out.push(0);
            } else if raw_data_counter == MAX_UNCOMPRESSED_BYTES {
                out[raw_data_counter_ptr] = 0x7F;
                raw_data_counter = 0;
            }

            out.push(data[r]);
            r += 1;
        }

        if raw_data_counter > 0 {
            out[raw_data_counter_ptr] = (raw_data_counter - 1) as u8;
        }

        Some(out)
    }

    pub fn calculate_checksum(data: &[u8]) -> u32 {
        data.iter().fold(0u32, |sum, &b| sum.wrapping_add(b as u32))
    }

    /// `compress`'in ürettiği akışı geri çözer (yalnızca testler için).
    #[cfg(test)]
    pub fn decompress(data: &[u8]) -> Vec<u8> {
        let uncomp_size = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let mut ptr_array = [0u16; TABLE_PTR_SIZE];
        for (i, ptr) in ptr_array.iter_mut().enumerate() {
            let off = 4 + i * 2;
            *ptr = u16::from_le_bytes(data[off..off + 2].try_into().unwrap());
        }
        let table_end = 4 + TABLE_PTR_SIZE * 2;
        let mut out = Vec::new();
        out.extend_from_slice(&data[table_end..table_end + uncomp_size]);

        let mut pos = table_end + uncomp_size;
        while pos < data.len() {
            let b0 = data[pos];
            if b0 & COMPRESSION_FLAG != 0 {
                let b1 = data[pos + 1];
                let comp_len = ((b0 & 0x7F) as u16) | ((((b1 >> 6) & 0x3) as u16) << 7);
                let ptr_idx = (b1 & 0x3F) as usize;
                let offset = ptr_array[ptr_idx] as usize;
                let match_len = comp_len as usize + 3;
                // Kaynak ile hedef örtüşebilir (offset < match_len), bu yüzden
                // `extend_from_within` değil, bayt bayt kopyalama gerekiyor.
                let start = out.len() - offset;
                for ref_pos in start..start + match_len {
                    let b = out[ref_pos];
                    out.push(b);
                }
                pos += 2;
            } else {
                let count = (b0 as usize) + 1;
                pos += 1;
                out.extend_from_slice(&data[pos..pos + count]);
                pos += count;
            }
        }
        out
    }
}

// ============================================================================
// SPL2 / QPDL Akış Yöneticisi (SplStreamWriter)
// ============================================================================

/// PJL alanlarına (JOBNAME, USERNAME) izin verilen azami BAYT sayısı.
///
/// PJL yorumlayıcıları küçük, sabit boyutlu satır tamponları kullanır; sınır
/// gerçekçi bir başlığı kesmeden megabaytlarca metnin firmware'e gitmesini
/// engeller. `sanitize_pjl_field` çıktısı saf ASCII olduğu için karakter ve
/// bayt sayısı eşittir.
const MAX_PJL_FIELD_BYTES: usize = 128;

/// PJL alanına doğrudan yazılabilecek karakterler: yazdırılabilir ASCII
/// (0x20-0x7E), çift tırnak hariç — PJL'de kaçış olmadığı için tırnak
/// alıntılanmış alanı erken kapatırdı.
#[inline]
fn is_safe_pjl_ascii(c: char) -> bool {
    matches!(c, ' '..='~') && c != '"'
}

/// İstemciden gelen serbest metni (iş adı, kullanıcı adı) PJL alanına
/// yazılabilir hâle getirir.
///
/// PJL satır tabanlıdır ve alıntılanmış dizeler için kaçış mekanizması
/// yoktur: gömülü bir CR/LF ya da ESC (UEL'in başlangıcı) alanı kapatıp
/// ardından gelen metni yeni bir PJL komutu ya da işi olarak yorumlatabilir.
/// Süzgeç bir beyaz listedir ve `char` düzeyinde değil çıktı baytı düzeyinde
/// güvence verir: her bayt 0x20-0x7E aralığındadır, yani çok baytlı UTF-8'in
/// devam baytları (ör. 0x9B = C1 CSI) da sızamaz. Beyaz listede olmayan her
/// karakter atılır.
fn sanitize_pjl_field(input: &str) -> String {
    input
        .chars()
        .filter(|&c| is_safe_pjl_ascii(c))
        .take(MAX_PJL_FIELD_BYTES)
        .collect()
}

pub struct SplStreamWriter<W: Write> {
    writer: W,
    /// Sayfa içindeki bant sırası. QPDL kaydında 8 bitlik bir alandır; sayaç
    /// `u16` tutuluyor ki 257. bant sessizce 0'a sarmak yerine hata versin.
    current_band: u16,
    /// `begin_job` çağrıldı ama `end_job` henüz çağrılmadı mı? `Drop` bunu
    /// okuyarak yarıda kalan bir işin kapanış UEL'ini yazar.
    job_active: bool,
}

impl<W: Write> SplStreamWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            current_band: 0,
            job_active: false,
        }
    }

    /// PJL iş başlığını gönderir ve yazıcıyı QPDL diline sokar.
    ///
    /// Satırların sırası ve değerleri SpliX printer.cpp `sendPJLHeader` ile
    /// aynıdır. ML-2165'te otomatik dupleks yoktur ve PPD dupleks seçeneği
    /// sunmaz; işler her zaman tek taraflıdır (`DUPLEX=OFF`).
    pub fn begin_job(&mut self, config: &JobConfig) -> io::Result<()> {
        let pjl = format!(
            "@PJL DEFAULT SERVICEDATE={}\n\
             @PJL SET USERNAME=\"{}\"\n\
             @PJL SET JOBNAME=\"{}\"\n\
             @PJL DEFAULT POWERSAVE=ON\n\
             @PJL DEFAULT POWERSAVETIME=5\n\
             @PJL SET JAMRECOVERY=OFF\n\
             @PJL SET DUPLEX=OFF\n\
             @PJL SET PAPERTYPE={}\n\
             @PJL SET ALTITUDE=LOW\n\
             @PJL SET DENSITY=3\n\
             @PJL SET RET=NORMAL\n\
             @PJL ENTER LANGUAGE = QPDL\n",
            sanitize_pjl_field(&config.service_date),
            sanitize_pjl_field(&config.user_name),
            sanitize_pjl_field(&config.job_name),
            config.paper_type,
        );

        self.writer.write_all(PJL_UEL)?;
        self.writer.write_all(pjl.as_bytes())?;
        self.writer.flush()?;

        // Bu noktadan itibaren yazıcı QPDL dilindedir ve akış mutlaka bir
        // kapanış UEL'i ile bitmelidir.
        self.job_active = true;
        Ok(())
    }

    /// 17 baytlık QPDL sayfa başlığını gönderir (SpliX qpdl.cpp `renderPage`).
    pub fn begin_page(&mut self, config: &PageConfig) -> io::Result<()> {
        self.current_band = 0;

        let mut header = [0u8; 17];
        header[0x0] = 0x00; // Sayfa başlığı imzası
        header[0x1] = (config.resolution_y.dpi() / 100) as u8; // DİKEY çözünürlük / 100
        header[0x2..0x4].copy_from_slice(&config.copies.to_be_bytes());
        header[0x4] = config.paper_size as u8;
        header[0x5..0x7].copy_from_slice(&config.width_pixels.to_be_bytes());
        header[0x7..0x9].copy_from_slice(&config.height_pixels.to_be_bytes());
        header[0x9] = config.paper_source as u8;
        header[0xA] = 0x00; // unknownByte1
                            // Dupleks baytları, SpliX'in tek taraflı (Simplex) değerleri:
                            // `duplex = 1` (sezgiye aykırı ama SpliX'te Simplex 1'dir), `tumble = 0`.
        header[0xB] = 0x01;
        header[0xC] = 0x00;
        header[0xD] = 0x00; // unknownByte2
        header[0xE] = QPDL_VERSION;
        header[0xF] = 0x01; // Renk düzlemi sayısı (monokrom)
        header[0x10] = (config.resolution_x.dpi() / 100) as u8; // YATAY çözünürlük / 100

        self.writer.write_all(&header)?;
        Ok(())
    }

    /// Bir şeridi Algo 0x11 ile sıkıştırıp alt başlık ve sağlama toplamıyla yazar.
    ///
    /// `band_width_pixels`, yazıcının satır adımıdır ve tam bayt hizalı
    /// (`bant genişliği bayt * 8`) olmalıdır; aksi hâlde satırlar kayar.
    pub fn write_compressed_band(
        &mut self,
        band_width_pixels: u16,
        band_height_lines: u16,
        raw_bitmap: &[u8],
    ) -> io::Result<()> {
        let payload_bytes = Algo0x11::compress(raw_bitmap).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Algo 0x11 sıkıştırıcısı bant için payload üretemedi (boş bant verisi).",
            )
        })?;

        // Toplam veri boyutu: payload + 4 (sub-header sig) + 4 (checksum)
        let total_data_size = (payload_bytes.len() + 8) as u32;

        // Bugün en büyük sayfa (Legal @1200 DPI) 132 bant eder; sınır,
        // kâğıt tablosu ya da bant yüksekliği değişirse sessizce bozulmasın diye.
        let band_index = u8::try_from(self.current_band).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Sayfa 256 banttan fazlasını içeriyor: QPDL bant sırası alanı 8 bitlik.",
            )
        })?;

        // 1. Şerit başlığı (Record 0x0C - 11 bayt)
        let mut band_header = [0u8; 11];
        band_header[0x0] = 0x0C; // İmza
        band_header[0x1] = band_index;
        band_header[0x2..0x4].copy_from_slice(&band_width_pixels.to_be_bytes());
        band_header[0x4..0x6].copy_from_slice(&band_height_lines.to_be_bytes());
        band_header[0x6] = COMPRESSION_ALGO_0X11;
        band_header[0x7..0xB].copy_from_slice(&total_data_size.to_be_bytes());
        self.writer.write_all(&band_header)?;

        // 2. Alt başlık imzası, 3. sıkıştırılmış veri
        self.writer.write_all(&SUBHEADER_SIG_LE)?;
        self.writer.write_all(&payload_bytes)?;

        // 4. Sağlama toplamı (alt başlık + payload)
        let checksum = Algo0x11::calculate_checksum(&SUBHEADER_SIG_LE)
            .wrapping_add(Algo0x11::calculate_checksum(&payload_bytes));
        self.writer.write_all(&checksum.to_be_bytes())?;

        self.current_band += 1;
        Ok(())
    }

    /// Sayfa sonu (3 baytlık QPDL kaydı: [0x01, kopya MSB, kopya LSB])
    pub fn end_page(&mut self, copies: u16) -> io::Result<()> {
        self.writer.write_all(&[0x01])?;
        self.writer.write_all(&copies.to_be_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// İş sonu: kapanış UEL'i (SpliX printer.cpp `sendPJLFooter`; sona
    /// fazladan "\n" eklenmez).
    ///
    /// İdempotenttir: iş başlamamışsa ya da zaten kapatılmışsa hiçbir şey
    /// yazmaz, böylece `Drop` başarılı akışlarda ikinci bir UEL üretmez.
    pub fn end_job(&mut self) -> io::Result<()> {
        if !self.job_active {
            return Ok(());
        }
        // Bayrak yazmadan ÖNCE düşer: yazma başarısızsa (backend boruyu
        // kapattıysa) `Drop` aynı yazmayı tekrar denemesin.
        self.job_active = false;
        self.writer.write_all(PJL_END)?;
        self.writer.flush()?;
        Ok(())
    }
}

/// Yarıda kalan bir işi kapanış UEL'i ile sonlandıran güvenlik ağı.
///
/// Akış `@PJL ENTER LANGUAGE = QPDL`'den sonra UEL olmadan biterse yazıcı
/// tamamlanmamış bir kaydı bekler hâlde kalır ve sıradaki iş de bozulur. Her
/// hata yolu (`?`) writer'ı düşürdüğü için kapanış burada garanti edilir.
///
/// DİKKAT: `std::process::exit` `Drop`'ları çalıştırmaz; bu yüzden writer
/// `process_cups_raster_to_spl` içinde yerel yaşar ve hata `main`'e `?` ile
/// döner. SIGTERM'in de bu yoldan geçmesi için bkz. cancel.rs.
impl<W: Write> Drop for SplStreamWriter<W> {
    fn drop(&mut self) {
        if self.job_active {
            // Hatayı raporlayacak bir yer yok; akışı kapatmayı denemek yine
            // de hiç denememekten iyidir.
            let _ = self.end_job();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QPDL bant sırası alanı 8 bitliktir; 257. bant sessizce 0'a sarmak
    /// yerine hata vermeli. Bugün erişilemez (en kötü durum 170 bant), ama
    /// invaryant `MAX_POINTS`/`QPDL_BAND_HEIGHT` değiştiğinde bozulmasın diye
    /// pinleniyor.
    #[test]
    fn test_band_index_beyond_255_is_rejected_not_wrapped() {
        let mut out: Vec<u8> = Vec::new();
        let mut w = SplStreamWriter::new(&mut out);
        let band = vec![0u8; 8];
        for i in 0..=255u16 {
            w.write_compressed_band(64, 1, &band)
                .unwrap_or_else(|e| panic!("bant {} reddedildi: {}", i, e));
        }
        let err = w
            .write_compressed_band(64, 1, &band)
            .expect_err("257. bant hata vermeli");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("8 bitlik"), "{}", err);
    }

    /// Sayaç sayfa başına sıfırlanır: art arda gelen sayfalar birbirinin
    /// bant sırasını tüketmemeli.
    #[test]
    fn test_band_index_resets_on_each_page() {
        let mut out: Vec<u8> = Vec::new();
        let mut w = SplStreamWriter::new(&mut out);
        let band = vec![0u8; 8];
        for _ in 0..2 {
            w.begin_page(&PageConfig::default()).unwrap();
            for _ in 0..200 {
                w.write_compressed_band(64, 1, &band).unwrap();
            }
        }
    }

    /// Tanınmayan bir ölçü A4'e ya da başka bir QPDL koduna düşürülmemeli.
    #[test]
    fn test_unknown_paper_size_is_rejected_instead_of_falling_back_to_a4() {
        assert_eq!(
            SplPaperSize::from_dimensions_pt_exact(595, 842),
            Some(SplPaperSize::A4)
        );
        assert_eq!(SplPaperSize::from_dimensions_pt_exact(612, 936), None);
    }

    /// DPI dönüşümü yaklaşık değerleri sessizce 300/600/1200'e yuvarlamamalı;
    /// yalnızca PPD'nin sunduğu eksen değerleri ve doğrulanmış çiftler geçerli.
    #[test]
    fn test_resolution_mapping_is_exact_and_pair_aware() {
        assert_eq!(
            SplResolution::from_dpi_exact(300),
            Some(SplResolution::Dpi300)
        );
        assert_eq!(
            SplResolution::from_dpi_exact(600),
            Some(SplResolution::Dpi600)
        );
        assert_eq!(
            SplResolution::from_dpi_exact(1200),
            Some(SplResolution::Dpi1200)
        );
        for unsupported in [0, 299, 599, 601, 1199, 1201] {
            assert_eq!(SplResolution::from_dpi_exact(unsupported), None);
        }

        assert!(SplResolution::pair_is_supported(300, 300));
        assert!(SplResolution::pair_is_supported(600, 600));
        assert!(SplResolution::pair_is_supported(1200, 600));
        assert!(SplResolution::pair_is_supported(1200, 1200));
        assert!(!SplResolution::pair_is_supported(600, 1200));
        assert!(!SplResolution::pair_is_supported(300, 600));
    }

    #[test]
    fn test_service_date_conversion_handles_epoch_and_leap_day() {
        assert_eq!(service_date_from_unix_days(-1), "19691231");
        assert_eq!(service_date_from_unix_days(0), "19700101");
        assert_eq!(service_date_from_unix_days(11_016), "20000229");
    }

    /// Sıkıştırıcı payload üretemezse bant kaydı BOŞ payload'la yazılmamalı.
    #[test]
    fn test_empty_band_data_is_an_error_not_an_empty_record() {
        let mut out: Vec<u8> = Vec::new();
        {
            let mut w = SplStreamWriter::new(&mut out);
            let err = w
                .write_compressed_band(64, 1, &[])
                .expect_err("boş bant verisi hata vermeli");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
        // `begin_job` çağrılmadığı için `Drop` da bir şey yazmamalı.
        assert!(out.is_empty(), "hata durumunda hiçbir bayt yazılmamalı");
    }

    #[test]
    fn test_sanitize_pjl_field_strips_injection_chars() {
        assert_eq!(sanitize_pjl_field("Normal Title"), "Normal Title");
        // Çift tırnak: alıntılanmış alanı erken kapatabilir.
        assert_eq!(sanitize_pjl_field("a\"b"), "ab");
        // CR/LF: yeni bir PJL komut satırı başlatabilir.
        assert_eq!(sanitize_pjl_field("a\r\nb"), "ab");
        // ESC: Universal Exit Language (@PJL zarfını erken bitirir).
        assert_eq!(sanitize_pjl_field("a\x1bb"), "ab");
        // Tam enjeksiyon denemesi: yeni bir @PJL komutu eklemeye çalışıyor.
        let injected = sanitize_pjl_field("x\"\n@PJL DEFAULT PASSWORD=1234\n");
        assert!(!injected.contains('"'));
        assert!(!injected.contains('\n'));
        assert!(!injected.contains('\r'));
    }

    #[test]
    fn test_sanitize_pjl_field_strips_unicode_line_separators() {
        // U+2028/U+2029: is_control() bunları yakalamaz, ama bazı metin
        // işleyicileri satır sonu sayabilir.
        assert_eq!(sanitize_pjl_field("a\u{2028}b"), "ab");
        assert_eq!(sanitize_pjl_field("a\u{2029}b"), "ab");
    }

    #[test]
    fn test_sanitize_pjl_field_strips_bidi_override_spoofing() {
        // U+202E (RLO) ile "evil.exe" gibi bir dosya adı sahteciliği
        // tekniğinin PJL alanlarında kullanıcı/iş adını farklı göstermesi
        // engellenmeli.
        let spoofed = sanitize_pjl_field("Invoice\u{202E}cod.exe");
        assert!(!spoofed.contains('\u{202E}'));
        assert_eq!(spoofed, "Invoicecod.exe");

        // Diğer bidi gömme/izolasyon ve sıfır genişlikli karakterler de.
        for c in ['\u{202A}', '\u{2066}', '\u{200B}', '\u{FEFF}'] {
            let s = format!("a{}b", c);
            assert_eq!(sanitize_pjl_field(&s), "ab", "char {:?} süzülmedi", c);
        }
    }

    /// ASCII dışı her karakter atılır; çevresindeki metin korunur.
    #[test]
    fn test_sanitize_pjl_field_drops_non_ascii() {
        assert_eq!(sanitize_pjl_field("Öğrenci Başvurusu"), "renci Bavurusu");
        assert_eq!(sanitize_pjl_field("Rapor \u{1F600} 2026"), "Rapor  2026");
        assert_eq!(sanitize_pjl_field("会議 notes"), " notes");
    }

    /// Çıktının HER baytı 0x20-0x7E aralığında olmalı: `char` düzeyinde bir
    /// kara liste, çok baytlı UTF-8'in devam baytlarını kaçırırdı (`U+02DB`
    /// telde `CB 9B` olur ve `0x9B` = C1 CSI).
    #[test]
    fn test_sanitize_pjl_field_output_is_always_printable_ascii() {
        let hostile = "A\u{02DB}B\u{0219}C\u{1E9B}D\u{FF02}E\u{202E}F\u{1F4A9}";
        let out = sanitize_pjl_field(hostile);
        assert!(
            out.bytes().all(|b| (0x20..=0x7E).contains(&b)),
            "yazdırılamayan bayt sızdı: {:?}",
            out.as_bytes()
        );
        assert!(!out.as_bytes().contains(&0x9B), "C1 CSI baytı sızdı");
        assert!(!out.contains('"'));

        // Aynı garanti geniş bir Unicode taramasında da geçerli olmalı.
        for cp in (0u32..0x3000).chain(0x1F300..0x1F400) {
            if let Some(c) = char::from_u32(cp) {
                let s = format!("x{}y", c);
                let out = sanitize_pjl_field(&s);
                assert!(
                    out.bytes().all(|b| (0x20..=0x7E).contains(&b)) && !out.contains('"'),
                    "U+{:04X} güvensiz bayt üretti: {:?}",
                    cp,
                    out.as_bytes()
                );
            }
        }
    }

    /// Sınır BAYT cinsindendir; çok baytlı girdi de satırı uzatamaz.
    #[test]
    fn test_sanitize_pjl_field_enforces_max_byte_length() {
        let huge = "A".repeat(1_000_000);
        assert_eq!(sanitize_pjl_field(&huge).len(), MAX_PJL_FIELD_BYTES);

        // Çok baytlı girdi de aynı BAYT sınırına uymalı.
        for probe in ["Ö", "\u{1F600}", "ß", "ğ"] {
            let s = probe.repeat(4000);
            let out = sanitize_pjl_field(&s);
            assert!(
                out.len() <= MAX_PJL_FIELD_BYTES,
                "{:?} sınırı aştı: {} bayt",
                probe,
                out.len()
            );
        }

        let short = "short title";
        assert_eq!(sanitize_pjl_field(short), short);
    }

    /// Üretilen PJL satırının tamamı bayt sınırının makul bir katında kalmalı.
    #[test]
    fn test_pjl_lines_stay_short_with_multibyte_input() {
        let cfg = JobConfig {
            job_name: "\u{1F600}".repeat(400),
            user_name: "Ö".repeat(400),
            service_date: "20120101".to_string(),
            paper_type: PJL_PAPERTYPE_DEFAULT,
        };
        let mut out: Vec<u8> = Vec::new();
        {
            let mut w = SplStreamWriter::new(&mut out);
            w.begin_job(&cfg).unwrap();
            w.end_job().unwrap();
        }
        for line in out.split(|&b| b == b'\n') {
            assert!(
                line.len() <= MAX_PJL_FIELD_BYTES + 64,
                "aşırı uzun PJL satırı: {} bayt",
                line.len()
            );
        }
    }

    #[test]
    fn test_begin_job_blocks_pjl_line_injection() {
        // Kötü niyetli girdi: tırnak + CR/LF ile alıntılanmış alandan kaçıp
        // yeni bir "@PJL DEFAULT PASSWORD=..." satırı enjekte etmeye,
        // ayrıca ESC (UEL) ile PJL zarfını erken bitirip yeni bir zarf/iş
        // başlatmaya çalışıyor.
        let malicious = JobConfig {
            job_name: "Evil\"\n@PJL DEFAULT PASSWORD=1234".to_string(),
            user_name: "attacker\x1b%-12345X@PJL SET JOBNAME=\"hijacked".to_string(),
            service_date: "20120101".to_string(),
            paper_type: PJL_PAPERTYPE_DEFAULT,
        };
        let benign = JobConfig {
            job_name: "Benign".to_string(),
            user_name: "benign".to_string(),
            service_date: "20120101".to_string(),
            paper_type: PJL_PAPERTYPE_DEFAULT,
        };

        // İşler açıkça kapatılır: writer düşerken `Drop` kapanış UEL'ini
        // yazacağı için (bkz. test_drop_writes_closing_uel_for_unfinished_job)
        // iki akış da tam bir iş olsun, aşağıdaki UEL sayımı yalnızca
        // ENJEKTE EDİLMİŞ zarfları ölçsün.
        let mut out_malicious: Vec<u8> = Vec::new();
        {
            let mut w = SplStreamWriter::new(&mut out_malicious);
            w.begin_job(&malicious).unwrap();
            w.end_job().unwrap();
        }
        let mut out_benign: Vec<u8> = Vec::new();
        {
            let mut w = SplStreamWriter::new(&mut out_benign);
            w.begin_job(&benign).unwrap();
            w.end_job().unwrap();
        }

        // Kötü niyetli girdi, zararsız girdiyle AYNI sayıda PJL satırı
        // üretmeli: sanitizasyon başarısız olsaydı gömülü "\n" ekstra
        // satır(lar) enjekte ederdi.
        let lines_malicious = out_malicious.iter().filter(|&&b| b == b'\n').count();
        let lines_benign = out_benign.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(
            lines_malicious, lines_benign,
            "girdi ekstra PJL komut satırı enjekte edebildi"
        );

        // Akışta yalnızca işin kendi açılış ve kapanış UEL'i bulunmalı;
        // girdi üçüncü bir zarf (yeni bir iş başlangıcı) ekleyememeli.
        let uel_count = count_uel(&out_malicious);
        assert_eq!(uel_count, count_uel(&out_benign));
        assert_eq!(uel_count, 2, "girdi yeni bir UEL zarfı enjekte edebildi");
    }

    /// Akıştaki UEL (yeni PJL zarfı) sayısını döner.
    fn count_uel(stream: &[u8]) -> usize {
        stream
            .windows(PJL_UEL.len())
            .filter(|w| *w == PJL_UEL)
            .count()
    }

    /// `begin_job` çağrıldıktan sonra `end_job` çağrılmadan writer düşerse,
    /// `Drop` kapanış UEL'ini yazmalı.
    ///
    /// `begin_job` yazıcıyı `@PJL ENTER LANGUAGE = QPDL` ile QPDL diline
    /// sokar; akış UEL olmadan biterse yazıcı bu dilde asılı kalır ve sonraki
    /// iş de bozulur.
    #[test]
    fn test_drop_writes_closing_uel_for_unfinished_job() {
        let mut out: Vec<u8> = Vec::new();
        {
            let mut w = SplStreamWriter::new(&mut out);
            w.begin_job(&JobConfig::default()).unwrap();
            w.begin_page(&PageConfig::default()).unwrap();
            // end_job YOK: hata yolunu taklit ediyoruz.
        }
        assert!(
            out.ends_with(PJL_END),
            "yarım kalan iş kapanış UEL'i olmadan bitti"
        );
        assert_eq!(count_uel(&out), 2, "açılış + kapanış UEL'i beklenir");
    }

    /// `end_job` açıkça çağrıldıysa `Drop` ikinci bir UEL yazmamalı.
    #[test]
    fn test_drop_does_not_duplicate_uel_after_end_job() {
        let mut out: Vec<u8> = Vec::new();
        {
            let mut w = SplStreamWriter::new(&mut out);
            w.begin_job(&JobConfig::default()).unwrap();
            w.end_job().unwrap();
        }
        assert_eq!(count_uel(&out), 2, "Drop fazladan UEL yazdı");
    }

    /// `begin_job` hiç çağrılmadıysa `Drop` hiçbir şey yazmamalı: yazıcı QPDL
    /// diline hiç girmemiştir, yalnız başına bir UEL göndermenin anlamı yok.
    #[test]
    fn test_drop_writes_nothing_when_job_never_started() {
        let mut out: Vec<u8> = Vec::new();
        {
            let _w = SplStreamWriter::new(&mut out);
        }
        assert!(out.is_empty(), "iş başlamadan çıktı üretildi: {:?}", out);
    }

    /// Sayfa başlığındaki 16-bit genişlik/yükseklik alanları büyük-endian
    /// olarak, kırpılmadan yazılmalı.
    #[test]
    fn test_begin_page_encodes_16bit_dimensions() {
        let cfg = PageConfig {
            width_pixels: u16::MAX,
            height_pixels: 0xABCD,
            ..PageConfig::default()
        };
        let mut out: Vec<u8> = Vec::new();
        SplStreamWriter::new(&mut out).begin_page(&cfg).unwrap();

        assert_eq!(&out[0x5..0x7], &[0xFF, 0xFF], "genişlik kırpıldı");
        assert_eq!(&out[0x7..0x9], &[0xAB, 0xCD], "yükseklik kırpıldı");
        assert_eq!(out.len(), 17);
    }

    /// Sayfa başlığında `header[0x1]` DİKEY (Y), `header[0x10]` YATAY (X)
    /// çözünürlüğü taşır — bu sırada (SpliX qpdl.cpp `renderPage`).
    #[test]
    fn test_begin_page_maps_resolution_axes_to_correct_bytes() {
        // Asimetrik: X = 1200, Y = 600 (gerçek bir QPDL modu).
        let cfg = PageConfig {
            resolution_x: SplResolution::Dpi1200,
            resolution_y: SplResolution::Dpi600,
            ..PageConfig::default()
        };
        let mut out: Vec<u8> = Vec::new();
        SplStreamWriter::new(&mut out).begin_page(&cfg).unwrap();

        assert_eq!(
            out[0x1], 6,
            "header[0x1] DİKEY (Y) çözünürlük olmalı: 600/100"
        );
        assert_eq!(
            out[0x10], 12,
            "header[0x10] YATAY (X) çözünürlük olmalı: 1200/100"
        );

        // Ters yön: eksenlerin gerçekten ayrı taşındığını kanıtlar.
        let swapped = PageConfig {
            resolution_x: SplResolution::Dpi600,
            resolution_y: SplResolution::Dpi1200,
            ..PageConfig::default()
        };
        let mut out2: Vec<u8> = Vec::new();
        SplStreamWriter::new(&mut out2)
            .begin_page(&swapped)
            .unwrap();
        assert_eq!(out2[0x1], 12);
        assert_eq!(out2[0x10], 6);

        // Simetrik modlarda iki bayt eşit.
        for dpi in [
            SplResolution::Dpi300,
            SplResolution::Dpi600,
            SplResolution::Dpi1200,
        ] {
            let sym = PageConfig {
                resolution_x: dpi,
                resolution_y: dpi,
                ..PageConfig::default()
            };
            let mut o: Vec<u8> = Vec::new();
            SplStreamWriter::new(&mut o).begin_page(&sym).unwrap();
            assert_eq!(o[0x1], o[0x10]);
            assert_eq!(o[0x1] as u32, dpi.dpi() / 100);
        }
    }

    /// `0` "seçilmedi" demektir ve Auto'ya düşer; tanınmayan değerler sessizce
    /// bir koda dönüşmek yerine `None` verir.
    #[test]
    fn test_paper_source_from_media_position() {
        assert_eq!(
            SplPaperSource::from_media_position(0),
            Some(SplPaperSource::Auto)
        );
        assert_eq!(
            SplPaperSource::from_media_position(1),
            Some(SplPaperSource::Auto)
        );
        assert_eq!(
            SplPaperSource::from_media_position(2),
            Some(SplPaperSource::Manual)
        );
        // ML-2165'te olmayan kaynakların QPDL kodları (3=Multi, 4=Upper,
        // 5=Lower) de dahil, geri kalan her değer tanınmaz.
        for unknown in [3u32, 4, 5, 6, 99, u32::MAX] {
            assert_eq!(
                SplPaperSource::from_media_position(unknown),
                None,
                "{}",
                unknown
            );
        }
        // Kodlar QPDL sayfa başlığına ham olarak yazıldığı için sayısal
        // değerleri de sabitlensin.
        assert_eq!(SplPaperSource::Auto as u8, 1);
        assert_eq!(SplPaperSource::Manual as u8, 2);
    }

    /// PJL sözlüğü dışında hiçbir şey `PAPERTYPE` değerine dönüşemez.
    #[test]
    fn test_pjl_paper_type_only_accepts_printer_vocabulary() {
        for known in PJL_PAPER_TYPES {
            assert_eq!(pjl_paper_type(known), Some(known));
            // ASCII büyük/küçük harf farkı geri düşüşe yol açmamalı.
            assert_eq!(pjl_paper_type(&known.to_ascii_lowercase()), Some(known));
            // Baştaki/sondaki boşluklar (64 baytlık C dizesi) tolere edilmeli.
            assert_eq!(pjl_paper_type(&format!("  {}  ", known)), Some(known));
        }
        // Okunabilir adlar ("Plain", "Envelope") protokol değeri değildir.
        for unknown in ["", "Plain", "Envelope", "CardStock", "NORMAL\nEVIL", "EN V"] {
            assert_eq!(pjl_paper_type(unknown), None, "{:?} kabul edildi", unknown);
        }
    }

    /// `PAPERTYPE` satırı TIRNAKSIZDIR: değere yalnızca tablodaki sabitler
    /// girebildiği için satır hiçbir girdiyle bozulamaz.
    #[test]
    fn test_begin_job_emits_papertype_from_config() {
        for paper_type in PJL_PAPER_TYPES {
            let cfg = JobConfig {
                paper_type,
                ..JobConfig::default()
            };
            let mut out: Vec<u8> = Vec::new();
            {
                let mut w = SplStreamWriter::new(&mut out);
                w.begin_job(&cfg).unwrap();
                w.end_job().unwrap();
            }
            let text = String::from_utf8_lossy(&out).into_owned();
            assert!(
                text.contains(&format!("@PJL SET PAPERTYPE={}\n", paper_type)),
                "{} yazılmadı: {}",
                paper_type,
                text
            );
            // Satır sayısı sabit kalmalı: değer hiçbir zaman satır bölmez.
            // Simplex bir işte 12 satır var: SERVICEDATE, USERNAME, JOBNAME,
            // POWERSAVE, POWERSAVETIME, JAMRECOVERY, DUPLEX, PAPERTYPE,
            // ALTITUDE, DENSITY, RET, ENTER LANGUAGE.
            assert_eq!(
                out.iter().filter(|&&b| b == b'\n').count(),
                12,
                "PJL satır sayısı değişti ({})",
                paper_type
            );
        }
    }

    #[test]
    fn test_algo0x11_compression() {
        let mut sample = vec![0x00u8; 620 * 64];
        sample[100..500].fill(0xAA);
        let comp = Algo0x11::compress(&sample).unwrap();
        assert!(comp.len() < sample.len());
    }

    #[test]
    fn test_algo0x11_roundtrip_synthetic() {
        // Rastgele olmayan ama tekrarlı+değişken içerik: metin benzeri veri.
        let mut sample = vec![0u8; 620 * 128];
        for (i, b) in sample.iter_mut().enumerate() {
            *b = ((i * 37 + (i / 620) * 13) % 251) as u8;
            if (i / 620) % 5 == 0 && (i % 620) > 50 && (i % 620) < 400 {
                *b = 0xFF;
            }
        }
        let comp = Algo0x11::compress(&sample).unwrap();
        let decomp = Algo0x11::decompress(&comp);
        assert_eq!(decomp.len(), sample.len(), "decompressed length mismatch");
        assert_eq!(decomp, sample, "decompressed content mismatch");
    }
}
