mod cancel;
mod raster;
mod spl;

use std::env;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};

use raster::{CupsColorOrder, CupsColorSpace, CupsRasterReader, PageHeader};
use spl::{
    current_service_date, JobConfig, PageConfig, SplPaperSize, SplPaperSource, SplResolution,
    SplStreamWriter,
};

/// CUPS filtre argümanları: `filter job-id user title num-copies options [filename]`
///
/// `num-copies` ve `options` kasıtlı olarak okunmuyor: zincirde bu filtreden
/// önce çalışan cups-filters aşaması (`gstoraster`/`pdftoraster`) PPD'yi
/// yorumlayıp medyayı, çözünürlüğü ve kopya sayısını raster SAYFA BAŞLIĞINA
/// yazar ve sayfa verisi ona göre üretilir. Komut satırıyla çeliştiğinde
/// bağlayıcı olan başlıktır.
#[derive(Default)]
struct CupsFilterArgs {
    job_id: Option<String>,
    user: Option<String>,
    title: Option<String>,
    filename: Option<String>,
}

impl CupsFilterArgs {
    fn parse(args: &[String]) -> Self {
        if args.len() >= 6 {
            Self {
                job_id: Some(args[1].clone()),
                user: Some(args[2].clone()),
                title: Some(args[3].clone()),
                filename: args.get(6).cloned(),
            }
        } else if args.len() == 2 && !args[1].starts_with('-') {
            // Doğrudan dosya modu: `cargo run -- dosya.raster`
            Self {
                filename: Some(args[1].clone()),
                ..Self::default()
            }
        } else {
            Self::default()
        }
    }
}

fn main() {
    // Argümanlar işi gönderen istemciden türetilir (ör. job-name) ve geçersiz
    // UTF-8 içerebilir; `env::args()` buna panic ile tepki verirdi.
    let raw_args: Vec<String> = env::args_os()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    // Argümansız çalıştırma gerçek bir `cupsd` çağrısı değildir. CUPS'ın kendi
    // filtreleri (`rastertopwg`, `pstops`) de bu durumda kullanım mesajı basıp
    // 1 ile çıkar; MIME türlerini listeleyip 0 ile çıkmak backend'lere özgüdür.
    if raw_args.len() <= 1 {
        let prog = raw_args
            .first()
            .cloned()
            .unwrap_or_else(|| "rastertospl-rust".to_string());
        eprintln!("Usage: {} job-id user title copies options [file]", prog);
        process::exit(1);
    }

    let args = CupsFilterArgs::parse(&raw_args);

    // Yazıcıya ilk bayt gitmeden kurulmalı; bkz. cancel.rs. Kurulamazsa iş
    // yine basılır, yalnızca iptal anında sonlanmaya döner.
    if let Err(err) = cancel::install_sigterm_handler() {
        eprintln!(
            "WARNING: SIGTERM işleyicisi kurulamadı ({}); iptal edilen bir iş yazıcıyı \
             yarım bir QPDL akışıyla bırakabilir.",
            err
        );
    }

    if let (Some(job), Some(user)) = (&args.job_id, &args.user) {
        eprintln!(
            "DEBUG: CUPS Job ID: {}, User: {}",
            quote_untrusted(job),
            quote_untrusted(user)
        );
    }
    if let Some(title) = &args.title {
        eprintln!("DEBUG: CUPS Title: {}", quote_untrusted(title));
    }

    let input_reader: Box<dyn Read> = match &args.filename {
        Some(path) => {
            eprintln!(
                "DEBUG: CUPS Raster dosyadan okunuyor: {}",
                quote_untrusted(path)
            );
            match File::open(path) {
                Ok(file) => Box::new(BufReader::new(file)),
                Err(err) => {
                    eprintln!(
                        "ERROR: Raster dosyası açılamadı {}: {}",
                        quote_untrusted(path),
                        err
                    );
                    process::exit(1);
                }
            }
        }
        None => {
            eprintln!("DEBUG: CUPS Raster standart girdiden (stdin) okunuyor");
            Box::new(BufReader::new(io::stdin()))
        }
    };

    // Writer `process_cups_raster_to_spl` içinde yerel yaşar: hata buraya
    // döndüğünde writer çoktan düşmüş ve `Drop` kapanış UEL'ini yazmıştır.
    // `process::exit` `Drop` çalıştırmadığı için sıralama önemlidir.
    let result = process_cups_raster_to_spl(&args, input_reader, io::stdout(), &cancel::REQUESTED);
    if let Err(err) = result {
        // İptalde dönen hata `cancel::check`'ten değil, EOF'tan da gelebilir:
        // zincirdeki önceki filtre de SIGTERM alıp çıkar. Bayrak kalkmışsa iş
        // başarısız sayılmaz; CUPS'ın `rastertoepson`'u da iptalde 0 ile çıkar.
        if cancel::REQUESTED.load(Ordering::Relaxed) {
            eprintln!(
                "INFO: İş iptal edildi; kalan bantlar gönderilmedi ve akışın sonuna \
                 kapanış UEL'i yazıldı (backend bağlantısı hâlâ açıksa) ({}).",
                err
            );
            return;
        }
        eprintln!("ERROR: Raster işleme hatası: {}", err);
        process::exit(1);
    }
}

/// Güvenilmez bir dizeyi (argv, raster başlığındaki metinler, dosya yolları)
/// log satırına gömülmeye hazırlar.
///
/// `{:?}` dizeyi tırnaklar ve kontrol karakterlerini kaçırır: gömülü CR/LF
/// `/var/log/cups/error_log`'a sahte satır ekleyemez, ANSI/OSC dizileri logu
/// izleyen yöneticinin terminalini manipüle edemez.
fn quote_untrusted(value: &str) -> String {
    format!("{:?}", value)
}

/// Fiziksel sayfa genişliğinin üzerinde kabul edilen yuvarlama payı (bayt).
const LINE_OVERSHOOT_SLACK_BYTES: u32 = 1;

/// Fiziksel sayfa yüksekliğinin üzerinde kabul edilen yuvarlama payı (satır).
const HEIGHT_OVERSHOOT_SLACK_LINES: u32 = 8;

/// Doğrulanmış bir sayfa başlığının QPDL karşılıkları. Yalnızca
/// `validate_page_header` üretir: kağıt kodu ve çözünürlük, doğrulamadan
/// geçmemiş bir başlıktan türetilemez ve ikisi birbirinden ayrışamaz.
#[derive(Debug)]
struct PageGeometry {
    paper_size: SplPaperSize,
    resolution_x: SplResolution,
    resolution_y: SplResolution,
}

/// Sayfa başlığını, bu filtrenin ve yazıcının kabul edebileceği geometriye
/// karşı doğrular ve QPDL karşılıklarını döndürür.
///
/// Başlık alanları doğrudan tampon boyutlarını ve QPDL sayfa başlığını
/// belirler; bozuk bir başlık devasa tahsislere ya da yazıcıya bildirilen
/// geometriyle gönderilen verinin ayrışmasına yol açardı.
///
/// Ayrı bir üst sınır sabiti yoktur: çözünürlük ve kâğıt ölçüsü yalnızca
/// PPD'nin sunduğu değerler olabilir, satır genişliği ve yüksekliği de o
/// kâğıdın fiziksel boyutuna sığmak zorundadır. En büyük kabul edilen sayfa
/// Legal @1200 DPI'dır: yatayda 2101 bayt/satır, dikeyde 16808 satır. Bunların
/// QPDL kayıtlarının alanlarına sığdığını `test_limits_cover_every_ppd_option`
/// denetler.
fn validate_page_header(header: &PageHeader) -> io::Result<PageGeometry> {
    let invalid = |msg: String| Err(io::Error::new(io::ErrorKind::InvalidData, msg));

    // Üst sınırlar aşağıda, sayfanın fiziksel boyutuna karşı denetlenir.
    if header.bytes_per_line == 0 {
        return invalid(format!(
            "Geçersiz cupsBytesPerLine değeri: {}",
            header.bytes_per_line
        ));
    }
    if header.height == 0 {
        return invalid(format!(
            "Geçersiz sayfa yüksekliği (satır sayısı): {}",
            header.height
        ));
    }
    let Some((resolution_x, resolution_y)) =
        SplResolution::from_dpi_pair(header.hw_resolution[0], header.hw_resolution[1])
    else {
        return invalid(format!(
            "Desteklenmeyen çözünürlük: {}x{} DPI (desteklenenler: 300x300, 600x600, 1200x600, 1200x1200)",
            header.hw_resolution[0], header.hw_resolution[1]
        ));
    };
    let Some(paper_size) = SplPaperSize::from_dimensions_pt_exact(
        header.page_size_points[0],
        header.page_size_points[1],
    ) else {
        return invalid(format!(
            "Desteklenmeyen kâğıt ölçüsü: {} x {} pt; QPDL kâğıt kodu ile raster geometrisinin uyuşması gerekir",
            header.page_size_points[0], header.page_size_points[1]
        ));
    };

    // Sol kenar boşluğu yatay yerleşimi belirler (bkz. `band_placement`).
    // Sayfadan geniş bir değer anlamsızdır ve `hard_margin_bytes`taki
    // toplamayı taşırabilirdi; sayfa genişliği yukarıda kâğıt tablosuyla
    // sınırlandığı için bu kontrol taşmayı da kapatır. PPD'deki değer 12,5 pt'dir, CUPS onu 12'ye
    // kırpar.
    if header.margins[0] >= header.page_size_points[0] {
        return invalid(format!(
            "Geçersiz sol kenar boşluğu: {} pt; sayfa genişliğinden ({} pt) küçük olmalı",
            header.margins[0], header.page_size_points[0]
        ));
    }

    // QPDL motoru tek düzlemli 1-bit monokrom (K) raster bekler ve
    // `stream_page_bands` her baytı öyle yorumlayıp tersler. Başka bir düzen
    // (ör. 24-bit RGB) şerit hizalamasını bozardı.
    if header.color_space != CupsColorSpace::K {
        return invalid(format!(
            "Desteklenmeyen renk uzayı: {} (yalnızca 1-bit K/monokrom destekleniyor)",
            header.color_space
        ));
    }
    if header.bits_per_color != 1 || header.bits_per_pixel != 1 {
        return invalid(format!(
            "Desteklenmeyen bit derinliği: bitsPerColor={}, bitsPerPixel={} (yalnızca 1-bit monokrom destekleniyor)",
            header.bits_per_color, header.bits_per_pixel
        ));
    }
    // Piksel 1 bit olduğuna göre satır `ceil(cupsWidth / 8)` bayttır.
    let expected_bytes_per_line = header.width.div_ceil(8);
    if expected_bytes_per_line != header.bytes_per_line {
        return invalid(format!(
            "cupsBytesPerLine ({}) cupsWidth ({}) ile tutarsız (beklenen: {})",
            header.bytes_per_line, header.width, expected_bytes_per_line
        ));
    }

    // 1-bit tek kanallı veride Chunked/Banded/Planar aynıdır; tanınmayan bir
    // değer ise üreticinin başka bir düzen kullandığını gösterir.
    if let CupsColorOrder::Unknown(order) = header.color_order {
        return invalid(format!(
            "Tanınmayan cupsColorOrder değeri: {} (beklenen: 0=Chunked, 1=Banded, 2=Planar)",
            order
        ));
    }

    // Satır, sayfanın FİZİKSEL genişliğine sığmalı. Yukarıdaki `cupsWidth`
    // kontrolü bunu yakalamaz: kendi içinde tutarlı ama sayfayla tutarsız bir
    // başlık (ör. `PageSize = 1 pt` + 620 B/satır) bandı daraltıp satırların
    // neredeyse tamamını attırırdı. Bant genişliği 8'e yukarı hizalandığı için
    // normalde satır zaten sığar; 1 baytlık pay yalnızca üreticinin farklı
    // yuvarlamasını karşılar.
    let band_width_bytes =
        compute_page_width_pixels(header.page_size_points[0], header.hw_resolution[0]).div_ceil(8);
    if header.bytes_per_line > band_width_bytes + LINE_OVERSHOOT_SLACK_BYTES {
        return invalid(format!(
            "cupsBytesPerLine ({}) sayfa genişliğine sığmıyor: {} pt @ {} DPI => en fazla {} bayt/satır",
            header.bytes_per_line,
            header.page_size_points[0],
            header.hw_resolution[0],
            band_width_bytes
        ));
    }

    // Aynı kural dikeyde: aksi hâlde yazıcıya sayfaya sığandan çok daha fazla
    // satır bildirilip gönderilebilirdi. Sayfanın fiziksel yüksekliği dikey
    // çözünürlükle hesaplanır (PPD `1200x600dpi` sunar) ve 8'e hizalanmaz; pay
    // bu yüzden 8 satırdır. Gerçek çıktı bu sınırın çok altındadır, çünkü
    // PPD'nin `*ImageableArea` kenar boşlukları düşer: A4 @600 DPI'da fiziksel
    // 7017 satıra karşılık ölçülen `cupsHeight` ~6817'dir.
    let page_height_lines = points_to_pixels(header.page_size_points[1], header.hw_resolution[1]);
    if header.height > page_height_lines + HEIGHT_OVERSHOOT_SLACK_LINES {
        return invalid(format!(
            "cupsHeight ({}) sayfa yüksekliğine sığmıyor: {} pt @ {} DPI => en fazla {} satır",
            header.height, header.page_size_points[1], header.hw_resolution[1], page_height_lines
        ));
    }

    Ok(PageGeometry {
        paper_size,
        resolution_x,
        resolution_y,
    })
}

/// Punto cinsinden bir uzunluğun verilen çözünürlükteki piksel karşılığı,
/// yukarı yuvarlanmış (SpliX `ceil(convertToXResolution(pt))`).
fn points_to_pixels(pt: u32, dpi: u32) -> u32 {
    (pt as f64 * dpi as f64 / 72.0).ceil() as u32
}

/// SpliX document.cpp `pageWidth`:
/// `((unsigned long)ceil(convertToXResolution(pageWidth)) + 7) & ~7`
fn compute_page_width_pixels(page_size_pt: u32, x_dpi: u32) -> u32 {
    (points_to_pixels(page_size_pt, x_dpi) + 7) & !7
}

/// Yazıcının sert kenar boşluğu, bant tamponu baytı olarak (SpliX
/// compress.cpp `_compressBandedPage`):
///
/// ```c
/// hardMarginX = ((unsigned long)ceil(convertToXResolution(hardMarginX())) + 7) & ~7;
/// hardMarginXInB = hardMarginX / 8;
/// ```
///
/// SpliX değeri PPD'nin `*ImageableArea` sol kenarından alır; bu filtre aynı
/// değeri raster başlığının `Margins[0]` alanından okur (12,5 pt, CUPS
/// tarafından 12'ye kırpılmış). 8'e hizalama şarttır: 12 pt @600 DPI = 100
/// piksel -> 104 piksel = 13 bayt; bant tamponu bayt adreslidir.
fn hard_margin_bytes(margin_pt: u32, x_dpi: u32) -> usize {
    (((points_to_pixels(margin_pt, x_dpi) + 7) & !7) / 8) as usize
}

/// CUPS satırının bant tamponundaki yatay yerleşimi. İki alan birlikte tek
/// bir işaretli ofseti temsil eder ve ikisi aynı anda sıfırdan büyük olamaz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BandPlacement {
    /// İçeriğin bant tamponunda başladığı sütun (bayt).
    dst_offset: usize,
    /// CUPS satırının başından atlanacak bayt sayısı.
    src_skip: usize,
}

/// Satırın bant tamponundaki konumunu SpliX ile aynı şekilde hesaplar:
///
/// ```c
/// // document.cpp — satırı sayfa genişliğinde ortala
/// marginWidthInB = (pageWidthInB - lineSize) / 2;
/// // compress.cpp — bandı doldururken sert kenar boşluğunu ATLA
/// band[x * bandHeight + y] = planes[i][index + x + hardMarginXInB + ...];
/// ```
///
/// Net ofset `ortalama - hardMarginXInB`'dir. A4 @600 DPI'da ortalama
/// `(620 - 595) / 2 = 12` bayt, sert kenar boşluğu 13 bayt: içerik bandın 0.
/// sütunundan başlar. Dikeyde de net sıfırdır (ortalama 100 satır, sert kenar
/// boşluğu 100 satır), bu yüzden sayfa döngüsü ilk satırı bandın 0. satırına
/// yazar.
fn band_placement(
    band_width_bytes: usize,
    cups_line_bytes: usize,
    hard_margin_bytes: usize,
) -> io::Result<BandPlacement> {
    let centered = band_width_bytes.saturating_sub(cups_line_bytes) / 2;
    let src_skip = hard_margin_bytes.saturating_sub(centered);

    // Satırın tamamını atlamak boş bir sayfa basmak demektir; sert kenar
    // boşluğu satırdan genişse geometri tutarsızdır ve reddedilir.
    if src_skip >= cups_line_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Sert kenar boşluğu ({} B) satır genişliğini ({} B) aşıyor: \
                 bant {} B, ortalama {} B; basılacak içerik kalmıyor",
                hard_margin_bytes, cups_line_bytes, band_width_bytes, centered
            ),
        ));
    }

    Ok(BandPlacement {
        dst_offset: centered.saturating_sub(hard_margin_bytes),
        src_skip,
    })
}

/// Raster başlığındaki `MediaType`'ı PJL `PAPERTYPE` değerine çevirir;
/// tanınmayan değerler `OFF`'a düşer ve uyarıyla bildirilir, böylece PPD ile
/// filtrenin kelime dağarcığı ayrışırsa (ör. eski bir PPD) log bunu gösterir.
fn pjl_paper_type_for(header: &PageHeader) -> &'static str {
    if header.media_type.is_empty() {
        return spl::PJL_PAPERTYPE_DEFAULT;
    }
    match spl::pjl_paper_type(&header.media_type) {
        Some(paper_type) => paper_type,
        None => {
            eprintln!(
                "WARNING: Tanınmayan MediaType {}; @PJL SET PAPERTYPE={} gönderiliyor. \
                 PPD'nin *MediaType anahtarları yazıcının PJL sözlüğünden olmalıdır: {}.",
                quote_untrusted(&header.media_type),
                spl::PJL_PAPERTYPE_DEFAULT,
                spl::PJL_PAPER_TYPES.join(", ")
            );
            spl::PJL_PAPERTYPE_DEFAULT
        }
    }
}

/// QPDL şerit yüksekliğinin temel değeri (SpliX PPD'lerindeki
/// `*QPDL BandSize: "128"`).
const QPDL_BAND_HEIGHT: usize = 128;

/// Bir sayfa için şerit yüksekliği. SpliX compress.cpp `_compressBandedPage`:
///
/// ```c
/// bandHeight = request.printer()->bandHeight();   // PPD: *QPDL BandSize
/// if (page->xResolution() == 300 && page->yResolution() == 300)
///     bandHeight /= 2;
/// ```
///
/// Yani yalnızca 300x300'de 64'tür; asimetrik `1200x600dpi` 128'de kalır. Bu
/// değer bant tamponunun boyutunu, transpoze indekslemeyi ve şerit kaydındaki
/// yükseklik alanını birlikte belirler.
fn band_height_for(header: &PageHeader) -> usize {
    if header.hw_resolution[0] == 300 && header.hw_resolution[1] == 300 {
        QPDL_BAND_HEIGHT / 2
    } else {
        QPDL_BAND_HEIGHT
    }
}

/// Bir sayfa için kabul edilen azami kopya sayısı. QPDL alanı 16 bittir;
/// 999, bozuk bir başlığın yazıcıyı saatlerce bastırmasına karşı bir paydır.
const MAX_REALISTIC_COPIES: u16 = 999;

/// Raster başlığındaki `num_copies` (u32) alanını QPDL'nin 16-bit alanına
/// sığdırır: 0 asla geçmez (`as u16` 65536'yı 0'a sarardı), aşırı değerler
/// `MAX_REALISTIC_COPIES`'e sabitlenir.
fn sanitize_copies(num_copies: u32) -> u16 {
    num_copies.clamp(1, MAX_REALISTIC_COPIES as u32) as u16
}

/// Standart CUPS Raster akışını okur ve Samsung QPDL/SPL2 formatına dönüştürür.
///
/// `writer` ve `cancel` parametre olarak alınır ki testler üretilen akışı
/// inceleyebilsin ve kendi iptal bayraklarını geçirebilsin; `main` süreç
/// geneli SIGTERM bayrağını (`cancel::REQUESTED`) geçirir. Bayrak her sayfa
/// ve her bant başında okunur; kalkmışsa iş, diğer hatalarla aynı yoldan
/// (`Drop` -> kapanış UEL'i) sonlanır.
fn process_cups_raster_to_spl<R: Read, W: Write>(
    args: &CupsFilterArgs,
    reader: R,
    writer: W,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut raster_reader = CupsRasterReader::new(reader)?;

    eprintln!(
        "INFO: Geçerli CUPS Raster akışı tespit edildi (Sürüm: {:?}, Endian: {})",
        raster_reader.version(),
        if raster_reader.version().is_big_endian() {
            "Big Endian"
        } else {
            "Little Endian"
        }
    );

    let mut spl_writer = SplStreamWriter::new(writer);

    // Kağıt türü PJL'de İŞ seviyesinde bildirilir, CUPS ise onu SAYFA
    // başlığında taşır; bu yüzden ilk sayfa başlığı iş başlamadan okunur.
    let mut next_header = raster_reader.next_page_header()?;

    let job_config = JobConfig {
        job_name: args
            .title
            .clone()
            .unwrap_or_else(|| "CUPS Document".to_string()),
        user_name: args.user.clone().unwrap_or_else(|| "guest".to_string()),
        service_date: current_service_date(),
        paper_type: next_header
            .as_ref()
            .map_or(spl::PJL_PAPERTYPE_DEFAULT, pjl_paper_type_for),
    };
    spl_writer.begin_job(&job_config)?;

    let mut page_number = 0;

    while let Some(header) = next_header.take() {
        cancel::check(cancel)?;
        let geometry = validate_page_header(&header)?;
        page_number += 1;

        // Kopya sayısı bir kez normalize edilir; log, sayfa başlığı ve sayfa
        // sonu aynı değeri kullanır.
        let copies = sanitize_copies(header.num_copies);

        eprintln!("PAGE: {} {}", page_number, copies);
        eprintln!("INFO: Sayfa {} başlatılıyor...", page_number);

        print_header_info(page_number, &header);

        // `cupsCompression` akış sıkıştırması değil, sürücüye özel bir ipucudur;
        // SpliX de bu filtre de onu kullanmaz. Sıfırdan farklıysa PPD ile
        // filtre arasında bir uyuşmazlık olabilir.
        if header.compression != 0 {
            eprintln!(
                "WARNING: cupsCompression={} yok sayıldı; bant sıkıştırması her zaman Algo 0x11 RLE.",
                header.compression
            );
        }

        // SpliX: pageWidth = ((ceil(pageSizePt * dpi / 72) + 7) & ~7),
        // bandWidthInB = (pageWidth + 7) / 8, bandWidth = bandWidthInB * 8.
        let page_width_pixels =
            compute_page_width_pixels(header.page_size_points[0], header.hw_resolution[0]);
        let band_width_bytes = page_width_pixels.div_ceil(8);
        let band_width_pixels = band_width_bytes * 8;

        // Sayfa ve bant kayıtlarındaki boyut alanları 16 bittir; doğrulama
        // sayesinde değerler zaten sığar, ama sessizce kırpmak yerine hata.
        let to_u16 = |value: u32, alan: &str| -> io::Result<u16> {
            u16::try_from(value).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} QPDL'nin 16-bit alanına sığmıyor: {} px", alan, value),
                )
            })
        };
        let band_width_u16 = to_u16(band_width_pixels, "Bant genişliği")?;
        let page_height_u16 = to_u16(header.height, "Sayfa yüksekliği")?;

        let cups_line_bytes = header.bytes_per_line as usize;
        let hard_margin = hard_margin_bytes(header.margins[0], header.hw_resolution[0]);
        let placement = band_placement(band_width_bytes as usize, cups_line_bytes, hard_margin)?;
        if (band_width_bytes as usize) < cups_line_bytes {
            // Bant satırdan darsa sağ kenar kırpılır; sert kenar boşluğu da
            // varsa satırın solundan da bayt atılır.
            let left_note = if placement.src_skip > 0 {
                format!(
                    " ve sert kenar boşluğu nedeniyle sol kenarından {} B atılacak",
                    placement.src_skip
                )
            } else {
                String::new()
            };
            eprintln!(
                "WARNING: Hesaplanan bant genişliği ({} B) CUPS satır genişliğinden ({} B) dar; \
                 satırların sağ kenarı kırpılacak{}.",
                band_width_bytes, cups_line_bytes, left_note
            );
        }

        eprintln!(
            "DEBUG: QPDL Genişlik: cupsWidth={}, pageWidthPx={}, bandWidthPx={}, bandWidthB={}, \
             hardMarginB={}, dstOffsetB={}, srcSkipB={}",
            header.width,
            page_width_pixels,
            band_width_pixels,
            band_width_bytes,
            hard_margin,
            placement.dst_offset,
            placement.src_skip
        );

        // Kağıt kaynağı PPD'nin `*InputSlot` seçeneğinden `MediaPosition`
        // üzerinden gelir.
        let paper_source = match SplPaperSource::from_media_position(header.media_position) {
            Some(source) => source,
            None => {
                eprintln!(
                    "WARNING: Tanınmayan MediaPosition değeri: {}; QPDL kağıt kaynağı \
                     Auto olarak gönderiliyor. PPD'nin *InputSlot seçenekleri QPDL \
                     kodlarıyla numaralandırılmalıdır (1=Auto, 2=Manual).",
                    header.media_position
                );
                SplPaperSource::Auto
            }
        };

        spl_writer.begin_page(&PageConfig {
            paper_size: geometry.paper_size,
            paper_source,
            resolution_x: geometry.resolution_x,
            resolution_y: geometry.resolution_y,
            copies,
            // SpliX qpdl.cpp renderPage: width = page->width() = pageWidth
            width_pixels: band_width_u16,
            height_pixels: page_height_u16,
        })?;

        stream_page_bands(
            &mut raster_reader,
            &mut spl_writer,
            &header,
            band_width_u16,
            placement,
            cancel,
        )?;

        spl_writer.end_page(copies)?;

        eprintln!("INFO: Sayfa {} tamamlandı.\n", page_number);

        next_header = raster_reader.next_page_header()?;
    }

    if page_number == 0 {
        eprintln!("WARNING: CUPS Raster akışında sayfa bulunamadı.");
    } else {
        eprintln!(
            "INFO: Toplam {} sayfa başarıyla SPL/QPDL formatına dönüştürüldü.",
            page_number
        );
    }

    // Sayfa bulunamamış olsa bile: `begin_job` yazıcıyı çoktan QPDL diline
    // soktuğu için akış her hâlükârda kapanış UEL'i ile bitmelidir.
    spl_writer.end_job()?;
    Ok(())
}

/// Bir sayfanın raster satırlarını SpliX uyumlu şeritler hâlinde yazar:
/// satırları `BandPlacement`e göre transpoze bant tamponuna kopyalar, tersler,
/// Algo 0x11 ile sıkıştırıp şerit kaydı olarak gönderir.
///
/// `band_width_pixels` tam bayt hizalıdır (bkz. `compute_page_width_pixels`).
fn stream_page_bands<R: Read, W: Write>(
    raster_reader: &mut CupsRasterReader<R>,
    spl_writer: &mut SplStreamWriter<W>,
    header: &PageHeader,
    band_width_pixels: u16,
    placement: BandPlacement,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let cups_bytes_per_line = header.bytes_per_line as usize;
    let total_lines = header.height as usize;
    let bw_bytes = band_width_pixels as usize / 8;

    let band_height = band_height_for(header);

    let mut line_buffer = vec![0u8; cups_bytes_per_line];

    // Satırın atlanan solundan sonra kalan kısım, bant tamponunda içeriğe
    // ayrılan yere sığmalı.
    let bytes_to_copy =
        (cups_bytes_per_line - placement.src_skip).min(bw_bytes - placement.dst_offset);

    // Tampon bir kez ayrılır ve her bandın başında sıfırlanır (son bant
    // eksik satırlı olabilir).
    let mut band_data = vec![0u8; bw_bytes * band_height];

    let mut current_line = 0;

    while current_line < total_lines {
        // İptal bant SINIRINDA ele alınır: yarım bir bant kaydı hiçbir zaman
        // yazılmaz. Bir bant en kötü durumda (sıkıştırılamaz gürültü) onlarca
        // milisaniye sürdüğü için gecikme önemsizdir.
        cancel::check(cancel)?;
        let lines_in_this_band = (total_lines - current_line).min(band_height);
        band_data.fill(0);

        for y in 0..lines_in_this_band {
            raster_reader.read_line(&mut line_buffer)?;

            // SpliX algo0x11.h: reverseLineColumn() == true, yani bant tamponu
            // SÜTUN öncelikli (transpoze) doldurulur:
            //   band[x * bandHeight + y] = planes[i][x + hardMarginXInB + ...]
            for (c, &byte) in line_buffer[placement.src_skip..]
                .iter()
                .take(bytes_to_copy)
                .enumerate()
            {
                let col = placement.dst_offset + c;
                band_data[col * band_height + y] = byte;
            }
        }

        // QPDL motoru CUPS K'nin TERSİ polariteyle çalışır: CUPS'ta 1 = siyah,
        // yazıcıda 0 = siyah. Terslemeden sayfa simsiyah çıkar.
        for b in &mut band_data {
            *b = !*b;
        }

        spl_writer.write_compressed_band(band_width_pixels, band_height as u16, &band_data)?;

        current_line += lines_in_this_band;
    }

    Ok(())
}

/// Sayfa başlığının teşhis bilgilerini `DEBUG:` önekiyle stderr'e basar.
/// CUPS öneksiz satırları da DEBUG sayar; önek niyeti açık eder.
fn print_header_info(page_num: u32, header: &PageHeader) {
    eprintln!("DEBUG: --------------------------------------------------");
    eprintln!("DEBUG:  [CUPS RASTER SAYFA {} META VERİLERİ]", page_num);
    eprintln!(
        "DEBUG:   Çözünürlük (DPI): {} x {}",
        header.hw_resolution[0], header.hw_resolution[1]
    );
    eprintln!(
        "DEBUG:   Boyutlar (px)   : {} x {} (Genişlik x Yükseklik)",
        header.width, header.height
    );
    eprintln!(
        "DEBUG:   Sayfa Boyutu(pt): {} x {} pt",
        header.page_size_points[0], header.page_size_points[1]
    );
    if let Some(name) = &header.page_size_name {
        eprintln!("DEBUG:   Medya Adı       : {}", quote_untrusted(name));
    }
    eprintln!("DEBUG:   Renk Uzayı      : {}", header.color_space);
    eprintln!("DEBUG:   Renk Dizilimi   : {:?}", header.color_order);
    eprintln!("DEBUG:   Kanal Bit Derin.: {}", header.bits_per_color);
    eprintln!("DEBUG:   Piksel Bit Der. : {}", header.bits_per_pixel);
    eprintln!("DEBUG:   Satır Bayt Say. : {} bayt", header.bytes_per_line);
    eprintln!(
        "DEBUG:   Ham Raster Boy. : {} bayt ({:.2} MB)",
        header.total_raster_bytes(),
        header.total_raster_bytes() as f64 / (1024.0 * 1024.0)
    );
    eprintln!("DEBUG:   Kopya Sayısı    : {}", header.num_copies);
    eprintln!(
        "DEBUG:   Kağıt Kaynağı   : MediaPosition={} -> {:?}",
        header.media_position,
        SplPaperSource::from_media_position(header.media_position)
    );
    // Tanınmayan değerin uyarısını `pjl_paper_type_for` iş başında bir kez
    // basar; burada yalnızca eşlemenin sonucu gösterilir.
    eprintln!(
        "DEBUG:   Kağıt Türü      : MediaType={} -> PAPERTYPE={}",
        quote_untrusted(&header.media_type),
        spl::pjl_paper_type(&header.media_type).unwrap_or(spl::PJL_PAPERTYPE_DEFAULT)
    );
    eprintln!("DEBUG: --------------------------------------------------");
}

#[cfg(test)]
mod tests {
    use super::*;
    use raster::CupsRasterVersion;
    use std::io::Cursor;

    /// Projenin PPD dosyasını okur. Aşağıdaki testlerin bir bölümü, filtrenin
    /// sabitlerini PPD'nin gerçek içeriğine bağlar: PPD'ye yeni bir seçenek
    /// eklendiğinde sabitleri güncellemeyi unutmak testi kırar.
    fn ppd_text() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ppd/samsung-ml2165.ppd"
        ))
        .expect("PPD okunamadı")
    }

    /// PPD'nin sunduğu `*Resolution` seçenekleri, (x_dpi, y_dpi) olarak.
    /// `600dpi` gibi tek değerli adlar iki eksene de yazılır.
    fn ppd_resolutions() -> Vec<(u32, u32)> {
        let ppd = ppd_text();
        let list: Vec<(u32, u32)> = ppd
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("*Resolution ")?;
                let name = rest.split('/').next().unwrap_or("").trim_end_matches("dpi");
                let parse = |v: &str| {
                    v.parse::<u32>()
                        .unwrap_or_else(|_| panic!("PPD çözünürlüğü ayrıştırılamadı: {}", line))
                };
                Some(match name.split_once('x') {
                    Some((x, y)) => (parse(x), parse(y)),
                    None => (parse(name), parse(name)),
                })
            })
            .collect();
        assert!(
            list.len() >= 4,
            "PPD'den çözünürlük okunamadı: {}",
            list.len()
        );
        list
    }

    /// PPD'nin sunduğu `*PaperDimension` seçenekleri, (ad, genişlik_pt,
    /// yükseklik_pt) olarak. Ondalık yazılmış ölçüler tavana yuvarlanır.
    fn ppd_paper_dimensions() -> Vec<(String, u32, u32)> {
        let ppd = ppd_text();
        let list: Vec<(String, u32, u32)> = ppd
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("*PaperDimension ")?;
                let (name, dims) = rest.split_once(':').expect("bozuk *PaperDimension satırı");
                let name = name.split('/').next().unwrap().trim().to_string();
                let mut it = dims.trim().trim_matches('"').split_whitespace();
                let mut pt = || {
                    it.next()
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or_else(|| panic!("PPD kağıt boyutu ayrıştırılamadı: {}", line))
                        .ceil() as u32
                };
                let (w, h) = (pt(), pt());
                Some((name, w, h))
            })
            .collect();
        assert!(
            list.len() >= 10,
            "PPD'den kağıt boyutu okunamadı: {}",
            list.len()
        );
        list
    }

    #[test]
    fn test_sanitize_copies_never_returns_zero() {
        assert_eq!(sanitize_copies(0), 1);
        assert_eq!(sanitize_copies(1), 1);
        assert_eq!(sanitize_copies(5), 5);
        assert_eq!(
            sanitize_copies(MAX_REALISTIC_COPIES as u32),
            MAX_REALISTIC_COPIES
        );
        // `as u16` 65536'yı 0'a sarardı.
        assert_eq!(sanitize_copies(65536), MAX_REALISTIC_COPIES);
        assert_eq!(sanitize_copies(131072), MAX_REALISTIC_COPIES);
        assert_eq!(sanitize_copies(u32::MAX), MAX_REALISTIC_COPIES);
    }

    /// A4, 600 DPI, 1-bit monokrom (K), 8x8 piksellik geçerli bir başlık
    /// üretir; testler bunu temel alıp tek bir alanı bozarak
    /// `validate_page_header`'ın onu reddettiğini doğrular.
    fn valid_header() -> PageHeader {
        let mut spec = RasterSpec::a4(600, 600, 8);
        spec.width_px = 8;
        PageHeader::parse(&spec.header_bytes(), CupsRasterVersion::V3Be).unwrap()
    }

    #[test]
    fn test_validate_page_header_accepts_valid_mono_header() {
        assert!(validate_page_header(&valid_header()).is_ok());
    }

    #[test]
    fn test_validate_page_header_rejects_unsupported_resolution_without_rounding() {
        for resolution in [[599, 599], [600, 1200], [300, 600], [1200, 300]] {
            let mut header = valid_header();
            header.hw_resolution = resolution;
            let err = validate_page_header(&header)
                .expect_err("PPD dışındaki çözünürlük reddedilmeliydi");
            assert!(
                err.to_string().contains("Desteklenmeyen çözünürlük"),
                "{}x{} için yanlış hata: {}",
                resolution[0],
                resolution[1],
                err
            );
        }
    }

    #[test]
    fn test_validate_page_header_rejects_unknown_paper_geometry() {
        let mut header = valid_header();
        header.page_size_points = [612, 936];
        let err = validate_page_header(&header)
            .expect_err("QPDL kodu olmayan kâğıt ölçüsü reddedilmeliydi");
        assert!(
            err.to_string().contains("Desteklenmeyen kâğıt ölçüsü"),
            "{}",
            err
        );
    }

    #[test]
    fn test_validate_page_header_rejects_non_k_color_space() {
        let mut header = valid_header();
        header.color_space = CupsColorSpace(1); // RGB
        assert!(validate_page_header(&header).is_err());
    }

    #[test]
    fn test_validate_page_header_rejects_wrong_bit_depth() {
        // 24-bit RGB / 32-bit CMYK gibi çok bitli bir akış simüle edilir.
        let mut header = valid_header();
        header.bits_per_color = 8;
        header.bits_per_pixel = 24;
        assert!(validate_page_header(&header).is_err());
    }

    #[test]
    fn test_validate_page_header_rejects_inconsistent_bytes_per_line() {
        let mut header = valid_header();
        header.bytes_per_line = 999; // width=8, bits_per_pixel=1 ile uyuşmuyor
        assert!(validate_page_header(&header).is_err());
    }

    /// Geçerli, tek sayfalık (A4 @600 DPI, 32x4 piksel), sıkıştırmasız bir V3
    /// Big-Endian raster akışı üretir. `pixel_bytes`, sayfa verisinin kaç
    /// baytının yazılacağını belirler; başlıkta bildirilen 16 bayttan az
    /// vermek kısa okuma (hata) yolunu tetikler.
    fn v3_stream(pixel_bytes: usize) -> Vec<u8> {
        let mut spec = RasterSpec::a4(600, 600, 4);
        spec.width_px = 32;
        let mut stream = spec.build();
        stream.truncate(4 + 1796 + pixel_bytes);
        stream
    }

    /// İstenen geometri ve seçeneklerle çok sayfalı, sıkıştırmasız (v3) bir
    /// CUPS Raster akışı kurar. Testlerdeki bütün raster başlıkları buradan
    /// üretilir.
    struct RasterSpec {
        res_x: u32,
        res_y: u32,
        page_pt: (u32, u32),
        width_px: u32,
        height: u32,
        pages: usize,
        /// CUPS `MediaPosition` (PPD `*InputSlot`).
        media_position: u32,
        /// CUPS `MediaType` (PPD `*MediaType`); boş = seçilmemiş.
        media_type: &'static str,
        /// CUPS `Margins[0]` (PPD `*ImageableArea`nın sol kenar boşluğu, pt).
        margin_left_pt: u32,
        /// Her raster satırı için kullanılacak desen; `None` = tamamen boş
        /// satır. Uzunluğu `bytes_per_line()` olmalıdır.
        line_pattern: Option<Vec<u8>>,
    }

    impl RasterSpec {
        /// A4, verilen çözünürlükte sayfa genişliğine tam oturan bir sayfa.
        fn a4(res_x: u32, res_y: u32, height: u32) -> Self {
            let width_px = compute_page_width_pixels(595, res_x);
            Self {
                res_x,
                res_y,
                page_pt: (595, 842),
                width_px,
                height,
                pages: 1,
                media_position: 0,
                media_type: "",
                margin_left_pt: 0,
                line_pattern: None,
            }
        }

        fn bytes_per_line(&self) -> u32 {
            self.width_px.div_ceil(8)
        }

        /// 1796 baytlık Big-Endian `cups_page_header2_t`.
        fn header_bytes(&self) -> Vec<u8> {
            let mut buf = vec![0u8; 1796];
            let mut put = |off: usize, val: u32| {
                buf[off..off + 4].copy_from_slice(&val.to_be_bytes());
            };
            put(276, self.res_x); // hw_resolution[0]
            put(280, self.res_y); // hw_resolution[1]
            put(312, self.margin_left_pt); // Margins[0] (sol, pt)
            put(324, self.media_position);
            put(352, self.page_pt.0); // page_size_points[0]
            put(356, self.page_pt.1); // page_size_points[1]
            put(372, self.width_px);
            put(376, self.height);
            put(384, 1); // bits_per_color
            put(388, 1); // bits_per_pixel
            put(392, self.bytes_per_line());
            put(400, 3); // color_space = K
            let media_type = self.media_type.as_bytes();
            buf[128..128 + media_type.len()].copy_from_slice(media_type);
            buf
        }

        fn build(&self) -> Vec<u8> {
            let header = self.header_bytes();
            let line = match &self.line_pattern {
                Some(pattern) => {
                    assert_eq!(
                        pattern.len(),
                        self.bytes_per_line() as usize,
                        "desen satır uzunluğuyla uyuşmuyor"
                    );
                    pattern.clone()
                }
                None => vec![0u8; self.bytes_per_line() as usize],
            };
            let mut stream = b"RaS3".to_vec();
            for _ in 0..self.pages {
                stream.extend_from_slice(&header);
                for _ in 0..self.height {
                    stream.extend_from_slice(&line);
                }
            }
            stream
        }
    }

    /// QPDL diline geçiş satırından hemen sonraki bayt konumu.
    fn qpdl_start(out: &[u8]) -> usize {
        const QPDL_MARK: &[u8] = b"ENTER LANGUAGE = QPDL\n";
        out.windows(QPDL_MARK.len())
            .position(|w| w == QPDL_MARK)
            .expect("QPDL diline geçiş satırı yok")
            + QPDL_MARK.len()
    }

    /// Üretilen SPL akışındaki İLK şerit kaydının payload'ını çözüp,
    /// `stream_page_bands`'in yazdığı bant tamponunu (terslemeden ÖNCEKİ
    /// hâliyle) geri verir.
    ///
    /// Tampon transpozedir: `band[col * band_height + y]`.
    fn first_band_buffer(out: &[u8]) -> Vec<u8> {
        // Kayıt konumu sabit ofsetten bulunur; 0x0C baytını aramak sayfa
        // başlığındaki bir değerle karışabilirdi.
        let pos = qpdl_start(out) + 17; // 17 baytlık sayfa başlığından sonrası
        assert_eq!(out[pos], 0x0C, "şerit kaydı imzası beklendi");
        assert_eq!(out[pos + 6], 0x11, "Algo 0x11 bekleniyordu");
        let total = u32::from_be_bytes(out[pos + 7..pos + 11].try_into().unwrap()) as usize;
        // 11 bayt kayıt başlığı + 4 bayt alt başlık; sondaki 4 bayt checksum.
        let payload = &out[pos + 15..pos + 11 + total - 4];
        let mut band = spl::Algo0x11::decompress(payload);
        // `stream_page_bands` yazmadan hemen önce tersliyor; geri al.
        for b in &mut band {
            *b = !*b;
        }
        band
    }

    /// Transpoze bant tamponunda, ilk satırdaki (`y == 0`) sıfır olmayan
    /// baytların sütun indislerini döner.
    fn nonzero_columns_in_first_line(band: &[u8], band_height: usize) -> Vec<usize> {
        band.chunks(band_height)
            .enumerate()
            .filter(|(_, col)| col[0] != 0)
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Üretilen SPL akışındaki tek bir şerit kaydının başlık alanları.
    #[derive(Debug, PartialEq, Eq)]
    struct BandRecord {
        index: u8,
        width_px: u16,
        height_lines: u16,
    }

    /// Üretilen SPL akışındaki tek bir sayfa: 17 baytlık başlık + şeritleri.
    #[derive(Debug)]
    struct SplPage {
        header: [u8; 17],
        bands: Vec<BandRecord>,
        /// Sayfa sonu kaydı var mı? İptal edilen bir işte son sayfa yarım kalır.
        complete: bool,
    }

    /// SPL çıktısını gerçek kayıt yapısına göre ayrıştırır; testler
    /// varsayımlarını değil tele yazılan baytları doğrulasın diye. Akış kapanış
    /// UEL'iyle bitmiyorsa ya da kayıtlar arasında artık bayt varsa panikler.
    fn parse_spl(out: &[u8]) -> Vec<SplPage> {
        let mut pages = Vec::new();
        let mut pos = qpdl_start(out);
        while pos < out.len() && !out[pos..].starts_with(spl::PJL_END) {
            assert_eq!(out[pos], 0x00, "sayfa başlığı imzası beklendi @ {}", pos);
            let mut header = [0u8; 17];
            header.copy_from_slice(&out[pos..pos + 17]);
            pos += 17;

            let mut bands = Vec::new();
            while pos < out.len() && out[pos] == 0x0C {
                let total = u32::from_be_bytes(out[pos + 7..pos + 11].try_into().unwrap()) as usize;
                bands.push(BandRecord {
                    index: out[pos + 1],
                    width_px: u16::from_be_bytes(out[pos + 2..pos + 4].try_into().unwrap()),
                    height_lines: u16::from_be_bytes(out[pos + 4..pos + 6].try_into().unwrap()),
                });
                // 11 baytlık kayıt başlığı + (alt başlık + payload + checksum)
                pos += 11 + total;
            }

            if out[pos..].starts_with(spl::PJL_END) {
                pages.push(SplPage {
                    header,
                    bands,
                    complete: false,
                });
                break;
            }
            assert_eq!(out[pos], 0x01, "sayfa sonu imzası beklendi @ {}", pos);
            pos += 3;
            pages.push(SplPage {
                header,
                bands,
                complete: true,
            });
        }
        assert_eq!(&out[pos..], spl::PJL_END, "akış kapanış UEL'iyle bitmeli");
        pages
    }

    /// Filtreyi bellekteki bir akış üzerinde, argümansız (doğrudan boru hattı)
    /// ve iptalsiz çalıştırır; sonucu ve yazıcıya giden baytları döner.
    ///
    /// İptal bayrağı yereldir: süreç geneli `cancel::REQUESTED`'ı kaldıran bir
    /// test, paralel çalışan diğer testleri de iptal ederdi.
    fn try_run_filter(stream: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        let mut out: Vec<u8> = Vec::new();
        let result = process_cups_raster_to_spl(
            &CupsFilterArgs::default(),
            Cursor::new(stream),
            &mut out,
            &AtomicBool::new(false),
        );
        (result, out)
    }

    /// `try_run_filter`'ın, akışın geçerli olmasını bekleyen hâli.
    fn run_filter(stream: Vec<u8>) -> Vec<u8> {
        let (result, out) = try_run_filter(stream);
        result.expect("filtre geçerli akışı işleyemedi");
        out
    }

    /// Çıktının başındaki PJL iş başlığı, metin olarak.
    fn pjl_text(out: &[u8]) -> String {
        String::from_utf8_lossy(&out[..qpdl_start(out)]).into_owned()
    }

    /// Girdinin ilk `threshold` baytı okunduğunda iptal bayrağını kaldıran
    /// okuyucu: SIGTERM'in işin TAM o noktasında gelmesini taklit eder.
    struct CancelAfter {
        inner: Cursor<Vec<u8>>,
        threshold: u64,
        flag: std::sync::Arc<AtomicBool>,
    }

    impl Read for CancelAfter {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            if self.inner.position() >= self.threshold {
                self.flag.store(true, Ordering::Relaxed);
            }
            Ok(n)
        }
    }

    /// Filtreyi, girdinin ilk `threshold` baytından sonra iptal edilecek
    /// şekilde çalıştırır; dönen hatayı ve üretilen akışı verir.
    fn run_cancelled_after(stream: Vec<u8>, threshold: u64) -> (io::Error, Vec<u8>) {
        let flag = std::sync::Arc::new(AtomicBool::new(threshold == 0));
        let reader = CancelAfter {
            inner: Cursor::new(stream),
            threshold,
            flag: flag.clone(),
        };
        let mut out: Vec<u8> = Vec::new();
        let err = process_cups_raster_to_spl(&CupsFilterArgs::default(), reader, &mut out, &flag)
            .expect_err("iptal edilen iş hata döndürmeliydi");
        (err, out)
    }

    fn count_uel(stream: &[u8]) -> usize {
        stream
            .windows(spl::PJL_UEL.len())
            .filter(|w| *w == spl::PJL_UEL)
            .count()
    }

    /// Sayfa verisi yarıda kesilirse dönüşüm hata döndürmeli, AMA yazıcıya
    /// giden akış yine de kapanış UEL'i ile bitmeli; aksi hâlde yazıcı QPDL
    /// dilinde yarım bir bant kaydını bekler hâlde kalır.
    #[test]
    fn test_closing_uel_written_on_truncated_page_data() {
        // 4 satır x 4 bayt = 16 bayt gerekiyordu
        let (result, out) = try_run_filter(v3_stream(2));
        let err = result.expect_err("kısa sayfa verisi hata döndürmeliydi");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            out.ends_with(spl::PJL_END),
            "hata yolu akışı kapanış UEL'i olmadan bıraktı"
        );
        assert_eq!(count_uel(&out), 2);
    }

    /// Sayfa başlığı doğrulamadan geçemezse de aynı garanti.
    #[test]
    fn test_closing_uel_written_on_invalid_page_header() {
        let mut stream = v3_stream(16);
        // bytes_per_line'ı 0 yap: validate_page_header reddedecek.
        stream[4 + 392..4 + 396].copy_from_slice(&0u32.to_be_bytes());

        let (result, out) = try_run_filter(stream);
        let err = result.expect_err("geçersiz başlık hata döndürmeliydi");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(out.ends_with(spl::PJL_END));
    }

    /// Akışta hiç sayfa yoksa da iş kapatılmalı: `begin_job` yazıcıyı çoktan
    /// QPDL diline sokmuştur.
    #[test]
    fn test_closing_uel_written_when_stream_has_no_pages() {
        let (result, out) = try_run_filter(b"RaS3".to_vec());
        result.expect("sayfasız akış hata değil, uyarı üretmeli");
        assert!(out.ends_with(spl::PJL_END));
        assert_eq!(count_uel(&out), 2);
    }

    /// Başarılı akış tam olarak iki UEL içermeli: `Drop`, açıkça kapatılmış
    /// bir işe üçüncü bir UEL eklememelidir.
    #[test]
    fn test_successful_stream_has_exactly_one_uel_pair() {
        let out = run_filter(v3_stream(16));
        assert!(out.starts_with(spl::PJL_UEL));
        assert!(out.ends_with(spl::PJL_END));
        assert_eq!(count_uel(&out), 2, "fazladan UEL yazıldı");
    }

    /// İlk sayfadan önce iptal: iş açılmış olsa da hiçbir sayfa kaydı
    /// yazılmamalı ve akış hemen kapanış UEL'i ile bitmeli.
    #[test]
    fn test_cancel_before_first_page_closes_job_without_pages() {
        let stream = RasterSpec::a4(600, 600, 256).build();
        let (err, out) = run_cancelled_after(stream, 0);
        assert!(err.to_string().contains("iptal"), "{}", err);

        assert!(parse_spl(&out).is_empty(), "iptalden sonra sayfa yazıldı");
    }

    /// Sayfa ortasında iptal, bant SINIRINDA ele alınmalı: o ana kadar
    /// tamamlanmış bantlar yazılır, yarım bant ve sayfa sonu kaydı yazılmaz,
    /// akış kapanış UEL'i ile biter.
    #[test]
    fn test_cancel_mid_page_stops_at_band_boundary() {
        // 512 satır = 600 DPI'da 128'lik 4 bant.
        let spec = RasterSpec::a4(600, 600, 512);
        let bpl = spec.bytes_per_line() as u64;
        // İptal 2. bandın ortasında (130. satır okunurken) gelir; 2. bant
        // tamamlanır, 3. bandın başındaki kontrol işi durdurur.
        let threshold = 4 + 1796 + 130 * bpl;
        let (err, out) = run_cancelled_after(spec.build(), threshold);
        assert!(err.to_string().contains("iptal"), "{}", err);

        let pages = parse_spl(&out);
        assert_eq!(pages.len(), 1);
        assert_eq!(
            pages[0].bands.len(),
            2,
            "iptalden sonra bant yazılmamalıydı"
        );
        assert!(!pages[0].complete, "yarım sayfaya sayfa sonu kaydı eklendi");
        assert_eq!(count_uel(&out), 2);
    }

    /// İki sayfa arasında iptal: tamamlanmış ilk sayfa olduğu gibi kalır
    /// (sayfa sonu kaydıyla), ikinci sayfa hiç başlatılmaz.
    #[test]
    fn test_cancel_between_pages_keeps_completed_page() {
        let mut spec = RasterSpec::a4(600, 600, 128);
        spec.pages = 2;
        let page_bytes = 1796 + 128 * spec.bytes_per_line() as u64;
        // İlk sayfanın son baytı okunduğu anda iptal.
        let (err, out) = run_cancelled_after(spec.build(), 4 + page_bytes);
        assert!(err.to_string().contains("iptal"), "{}", err);

        let pages = parse_spl(&out);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].bands.len(), 1);
        assert!(pages[0].complete);
        assert_eq!(count_uel(&out), 2);
    }

    /// Sıkıştırılmış v2 akışı, aynı içeriğin sıkıştırmasız v3 hâliyle BİRE BİR
    /// aynı SPL çıktısını üretmeli: `CupsLineDecoder` araya giriyor ve akışın
    /// sürümü çıktıyı etkilemiyor.
    #[test]
    fn test_v2_and_v3_streams_produce_identical_output() {
        // 4 bayt/satır, 3 satır: 0xAA 0xAA 0xAA 0x55 / 0x00 x4 / 0xFF x4
        let pixels: [&[u8]; 3] = [
            &[0xAA, 0xAA, 0xAA, 0x55],
            &[0x00, 0x00, 0x00, 0x00],
            &[0xFF, 0xFF, 0xFF, 0xFF],
        ];

        let mut spec = RasterSpec::a4(600, 600, 3);
        spec.width_px = 32;
        let hdr = spec.header_bytes();

        let mut v3 = b"RaS3".to_vec();
        v3.extend_from_slice(&hdr);
        for line in pixels {
            v3.extend_from_slice(line);
        }

        // Aynı içeriğin CUPS satır-RLE karşılığı.
        let mut v2 = b"RaS2".to_vec();
        v2.extend_from_slice(&hdr);
        v2.extend_from_slice(&[0x00, 0x02, 0xAA, 0x00, 0x55]); // 0xAA x3, 0x55 x1
        v2.extend_from_slice(&[0x00, 0x80]); // satır sonuna kadar boş (K => 0x00)
        v2.extend_from_slice(&[0x00, 0x03, 0xFF]); // 0xFF x4

        let out_v3 = run_filter(v3);
        let out_v2 = run_filter(v2);

        assert!(!out_v3.is_empty());
        assert_eq!(out_v2, out_v3, "v2 ve v3 çıktıları ayrıştı");
    }

    /// PPD'nin sunduğu HER kağıt ve çözünürlük, QPDL kayıtlarının alanlarına
    /// sığmalı.
    ///
    /// `validate_page_header` satır genişliğini ve yüksekliğini kâğıdın
    /// fiziksel boyutuyla sınırlar; QPDL ise genişliği ve yüksekliği 16 bitlik,
    /// bant sırasını 8 bitlik alanlarda taşır. PPD'ye daha büyük bir kağıt ya
    /// da daha yüksek bir çözünürlük eklenirse doğrulamadan geçen meşru bir iş
    /// bu alanlara sığmayıp çalışma anında reddedilirdi. Bu test o bağı
    /// zorunlu kılar.
    #[test]
    fn test_limits_cover_every_ppd_option() {
        for (x_dpi, y_dpi) in ppd_resolutions() {
            assert!(
                SplResolution::from_dpi_pair(x_dpi, y_dpi).is_some(),
                "PPD {}x{} DPI sunuyor ama filtre bu çifti desteklemiyor",
                x_dpi,
                y_dpi
            );
            let mut header = valid_header();
            header.hw_resolution = [x_dpi, y_dpi];
            let band_height = band_height_for(&header) as u32;

            for (name, w, h) in ppd_paper_dimensions() {
                // Dikey ve yatay (genişlik/yükseklik yer değiştirmiş) sayfa.
                for (width_pt, height_pt) in [(w, h), (h, w)] {
                    let what = format!(
                        "PPD '{}' {}x{} pt @ {}x{} DPI",
                        name, width_pt, height_pt, x_dpi, y_dpi
                    );

                    let width_px = compute_page_width_pixels(width_pt, x_dpi);
                    assert!(
                        u16::try_from(width_px).is_ok(),
                        "{}: bant genişliği {} px, 16-bit alana sığmıyor",
                        what,
                        width_px
                    );

                    // Doğrulamanın kabul ettiği en uzun sayfa.
                    let max_lines =
                        points_to_pixels(height_pt, y_dpi) + HEIGHT_OVERSHOOT_SLACK_LINES;
                    assert!(
                        u16::try_from(max_lines).is_ok(),
                        "{}: {} satır, 16-bit alana sığmıyor",
                        what,
                        max_lines
                    );
                    let bands = max_lines.div_ceil(band_height);
                    assert!(
                        bands <= 256,
                        "{}: {} bant, 8-bit bant sırasına sığmıyor",
                        what,
                        bands
                    );
                }
            }
        }
    }

    #[test]
    fn test_validate_page_header_rejects_line_wider_than_page() {
        let mut header = valid_header();
        header.page_size_points = [297, 420]; // desteklenen A6
        header.width = 4960;
        header.bytes_per_line = 620; // cupsWidth ile tutarlı, sayfayla değil
        let err = validate_page_header(&header).expect_err("dar sayfa reddedilmeliydi");
        assert!(
            err.to_string().contains("sayfa genişliğine sığmıyor"),
            "hata nedeni açıklanmalı: {}",
            err
        );
    }

    /// Gerçek bir tam genişlik A4 sayfası (595 pt @ 600 DPI = 620 B/satır)
    /// reddedilmemeli — aşırı düzeltme kontrolü.
    #[test]
    fn test_validate_page_header_accepts_full_width_a4() {
        let mut header = valid_header();
        header.width = 4960;
        header.bytes_per_line = 620;
        assert!(validate_page_header(&header).is_ok());
    }

    /// Yuvarlama payı: 1 baytlık aşım hoş görülür, 2 bayt reddedilir.
    #[test]
    fn test_validate_page_header_line_width_slack_is_one_byte() {
        // 595 pt @ 600 DPI => 4960 px => 620 bayt bant genişliği.
        let mut ok = valid_header();
        ok.width = 4968; // 621 bayt
        ok.bytes_per_line = 621;
        assert!(
            validate_page_header(&ok).is_ok(),
            "1 baytlık pay kabul edilmeli"
        );

        let mut too_wide = valid_header();
        too_wide.width = 4976; // 622 bayt
        too_wide.bytes_per_line = 622;
        assert!(
            validate_page_header(&too_wide).is_err(),
            "2 bayt aşım reddedilmeli"
        );
    }

    /// `cupsHeight` sayfanın fiziksel yüksekliğine sığmalı (genişlik
    /// kontrolünün dikey karşılığı).
    #[test]
    fn test_validate_page_header_rejects_page_taller_than_paper() {
        let mut header = valid_header();
        header.page_size_points = [297, 420]; // desteklenen A6
        header.height = 4_000; // A4'e sığardı, ama A6'ya sığmıyor
        let err = validate_page_header(&header).expect_err("uzun sayfa reddedilmeliydi");
        assert!(
            err.to_string().contains("sayfa yüksekliğine sığmıyor"),
            "hata nedeni açıklanmalı: {}",
            err
        );

        // Normal boyutlu bir A4 sayfasında da aşım yakalanmalı: 842 pt @ 600
        // DPI = 7017 satır; raster bunun altında kalmalıdır.
        let mut a4 = valid_header();
        a4.height = 24_000;
        assert!(
            validate_page_header(&a4).is_err(),
            "A4'e sığmayan yükseklik reddedilmeli"
        );
    }

    /// Aşırı düzeltme kontrolü: gerçek `cupsfilter` çıktısının ürettiği
    /// yükseklikler reddedilmemeli. Değerler, ppd/samsung-ml2165.ppd ile
    /// `cupsfilter -m application/vnd.cups-raster` çalıştırılarak ölçüldü;
    /// hepsi fiziksel sınırın altında kalır çünkü `*ImageableArea` kenar
    /// boşlukları (12,5 pt üst + 12,5 pt alt) düşülür. Tablo, kenar boşlukları
    /// 12 pt iken ölçüldü; 12,5 pt'lik güncel PPD birkaç satır daha kısa çıktı
    /// ürettiği için bu değerler kontrolü yalnızca daha da sıkı test eder.
    #[test]
    fn test_validate_page_header_accepts_real_cupsfilter_heights() {
        // (sayfa_genişliği_pt, sayfa_yüksekliği_pt, y_dpi, ölçülen cupsHeight)
        let measured = [
            (595u32, 842u32, 300u32, 3408u32), // A4
            (595, 842, 600, 6817),
            (595, 842, 1200, 13633),
            (612, 792, 600, 6400),  // Letter
            (612, 1008, 600, 8200), // Legal
            (612, 1008, 1200, 16400),
            (297, 420, 600, 3300),   // A6
            (595, 935, 1200, 15183), // Folio
        ];
        for (width_pt, height_pt, ydpi, cups_height) in measured {
            let mut h = valid_header();
            h.page_size_points = [width_pt, height_pt];
            // Bu tablodaki ölçümler simetrik çözünürlüklerden alındı.
            h.hw_resolution = [ydpi, ydpi];
            h.height = cups_height;
            assert!(
                validate_page_header(&h).is_ok(),
                "gerçek cupsfilter çıktısı reddedildi: {}x{} pt @ {} DPI => {} satır",
                width_pt,
                height_pt,
                ydpi,
                cups_height
            );
        }
    }

    /// Yuvarlama payı: 8 satırlık aşım hoş görülür, fazlası reddedilir.
    #[test]
    fn test_validate_page_header_height_slack_is_eight_lines() {
        // 842 pt @ 600 DPI => ceil(842 * 600 / 72) = 7017 satır.
        let exact = points_to_pixels(842, 600);
        assert_eq!(exact, 7017);

        let mut ok = valid_header();
        ok.height = exact + 8;
        assert!(
            validate_page_header(&ok).is_ok(),
            "8 satırlık pay kabul edilmeli"
        );

        let mut too_tall = valid_header();
        too_tall.height = exact + 9;
        assert!(
            validate_page_header(&too_tall).is_err(),
            "9 satır aşım reddedilmeli"
        );
    }

    /// Sert kenar boşluğu SpliX ile birebir aynı hesaplanmalı.
    ///
    /// SpliX compress.cpp: `((ceil(marginPt * dpi / 72) + 7) & ~7) / 8`.
    /// 8'e YUKARI hizalama önemli: 12 pt @600 DPI = 100 piksel, hizalanınca
    /// 104 piksel = 13 bayt olur; hizalamasız 12,5 bayt (kırpılınca 12) çıkar
    /// ve bant bir bayt kayar.
    #[test]
    fn test_hard_margin_matches_splix_alignment() {
        // PPD'deki *ImageableArea sol kenar boşluğu 12,5 pt; CUPS bunu
        // kırparak Margins[0] = 12 pt olarak yazar.
        assert_eq!(hard_margin_bytes(12, 600), 13);
        assert_eq!(hard_margin_bytes(12, 300), 7);
        assert_eq!(hard_margin_bytes(12, 1200), 25);
        // Kenar boşluğu bildirilmemişse kaydırma da yok.
        assert_eq!(hard_margin_bytes(0, 600), 0);
    }

    /// Yatay yerleşim ORTALAMA EKSİ SERT KENAR BOŞLUĞU olmalı, yalnızca
    /// ortalama değil: SpliX bandı doldururken `hardMarginXInB` kadar atlar.
    /// Yalnızca ortalama, A4 @600 DPI'da ~4 mm'lik bir sağa kayma olurdu.
    #[test]
    fn test_band_placement_subtracts_hard_margin() {
        // A4 @600 DPI, bu projenin PPD'sindeki gerçek sayılar:
        // bant 620 B, CUPS satırı 595 B, sert kenar boşluğu 13 B.
        let a4 = band_placement(620, 595, hard_margin_bytes(12, 600)).unwrap();
        assert_eq!(
            a4,
            BandPlacement {
                dst_offset: 0,
                src_skip: 1
            },
            "ortalama (12 B) sert kenar boşluğunu (13 B) telafi etmeli"
        );

        // Regresyon çapası: sert kenar boşluğu düşülmezse eski, hatalı
        // 12 baytlık kayma geri gelir.
        assert_eq!(
            band_placement(620, 595, 0).unwrap(),
            BandPlacement {
                dst_offset: 12,
                src_skip: 0
            },
            "kenar boşluğu yoksa davranış saf ortalamadır"
        );

        // Ortalama sert kenar boşluğundan BÜYÜKSE fark hedefe kalır.
        assert_eq!(
            band_placement(620, 560, 13).unwrap(),
            BandPlacement {
                dst_offset: 17,
                src_skip: 0
            }
        );

        // Bant satırdan darsa ortalama yoktur; kenar boşluğu satırdan atılır.
        assert_eq!(
            band_placement(600, 620, 13).unwrap(),
            BandPlacement {
                dst_offset: 0,
                src_skip: 13
            }
        );

        // Satırın tamamı atlanacaksa geometri tutarsızdır: kırpmak (fiilen
        // boş bir sayfa basmak) yerine reddedilir.
        let err = band_placement(4, 4, 999)
            .expect_err("satırdan geniş sert kenar boşluğu reddedilmeliydi");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("Sert kenar boşluğu"), "{}", err);

        // Sınır: tam olarak bir bayt kalması hâlâ kabul edilir.
        assert_eq!(
            band_placement(4, 4, 3).unwrap(),
            BandPlacement {
                dst_offset: 0,
                src_skip: 3
            }
        );
    }

    /// Sayfadan geniş bir sol kenar boşluğu başlık doğrulamasında reddedilir;
    /// aksi hâlde `hard_margin_bytes` içindeki `px + 7` toplaması taşabilirdi.
    #[test]
    fn test_validate_page_header_rejects_out_of_page_left_margin() {
        for margin in [595, 600, 300_000_000] {
            let mut header = valid_header();
            header.margins[0] = margin;
            let err = validate_page_header(&header)
                .expect_err("sayfadan geniş sol kenar boşluğu reddedilmeliydi");
            assert!(
                err.to_string().contains("Geçersiz sol kenar boşluğu"),
                "{} pt için yanlış hata: {}",
                margin,
                err
            );
        }

        // PPD'nin gerçek değeri (12,5 pt kırpılınca 12) ve sınırın hemen altı
        // kabul edilmeli.
        for margin in [0, 12, 594] {
            let mut header = valid_header();
            header.margins[0] = margin;
            assert!(
                validate_page_header(&header).is_ok(),
                "{} pt kabul edilmeliydi",
                margin
            );
        }
    }

    /// Uçtan uca: raster içeriği, yazıcıya giden bant tamponunda sert kenar
    /// boşluğu düşülmüş sütunda durmalı.
    ///
    /// Gerçek A4 @600 DPI geometrisi kurulur (620 B bant, 595 B CUPS satırı,
    /// `Margins[0] = 12 pt`) ve satırın 3. baytına bir işaret konur.
    /// Beklenen sütun `3 - src_skip + dst_offset = 2`'dir.
    #[test]
    fn test_content_lands_at_hard_margin_corrected_column() {
        const MARKER_INDEX: usize = 3;
        let mut spec = RasterSpec::a4(600, 600, 8);
        spec.width_px = 4760; // 595 B/satır: gerçek cupsfilter çıktısına eşit
        spec.margin_left_pt = 12; // PPD *ImageableArea: "12.5 12.5 582.5 829.5" -> 12

        let mut pattern = vec![0u8; 595];
        pattern[MARKER_INDEX] = 0xFF;
        spec.line_pattern = Some(pattern);

        let out = run_filter(spec.build());
        let band = first_band_buffer(&out);
        assert_eq!(band.len(), 620 * QPDL_BAND_HEIGHT, "bant tamponu boyutu");

        let columns = nonzero_columns_in_first_line(&band, QPDL_BAND_HEIGHT);
        assert_eq!(
            columns,
            vec![2],
            "işaret baytı yanlış sütunda; 15 ise sert kenar boşluğu düşülmüyor"
        );
    }

    /// Kenar boşluğu bildirmeyen bir akış (ör. PPD'siz üretilmiş raster) eski
    /// saf-ortalama davranışını korumalı: düzeltme `Margins[0]`'a bağlıdır ve
    /// alan yoksa bir şey uydurmaz.
    #[test]
    fn test_content_placement_without_margins_is_pure_centering() {
        const MARKER_INDEX: usize = 3;
        let mut spec = RasterSpec::a4(600, 600, 8);
        spec.width_px = 4760;
        spec.margin_left_pt = 0;

        let mut pattern = vec![0u8; 595];
        pattern[MARKER_INDEX] = 0xFF;
        spec.line_pattern = Some(pattern);

        let out = run_filter(spec.build());
        let band = first_band_buffer(&out);
        let columns = nonzero_columns_in_first_line(&band, QPDL_BAND_HEIGHT);
        assert_eq!(columns, vec![12 + MARKER_INDEX]);
    }

    /// Yatay ve dikey eksen AYNI origin'i kullanmalı.
    ///
    /// Hatanın özü buydu: dikeyde hiçbir kaydırma yokken yatayda 12 baytlık
    /// bir kaydırma vardı. SpliX'te iki eksenin de neti sıfırdır (ortalama
    /// eksi sert kenar boşluğu). Bu test ilk raster satırının bant tamponunun
    /// hem 0. satırında hem 0. sütununda başladığını sabitler.
    #[test]
    fn test_horizontal_and_vertical_origins_agree() {
        let mut spec = RasterSpec::a4(600, 600, 8);
        spec.width_px = 4760;
        spec.margin_left_pt = 12;
        // Satırın ilk baytı işaretli: src_skip = 1 olduğu için 1. bayt
        // 0. sütuna düşer.
        let mut pattern = vec![0u8; 595];
        pattern[1] = 0xFF;
        spec.line_pattern = Some(pattern);

        let out = run_filter(spec.build());
        let band = first_band_buffer(&out);

        // Sütun 0, satır 0: içerik bandın sol-üst köşesinden başlar.
        assert_eq!(band[0], 0xFF, "içerik bandın (0,0) köşesinde başlamalı");
        assert_eq!(
            nonzero_columns_in_first_line(&band, QPDL_BAND_HEIGHT),
            vec![0]
        );
    }

    /// Yükseklik sınırı dikey çözünürlüğe bağlı olmalı: `validate_page_header`
    /// sayfa yüksekliğini `hw_resolution[1]` ile hesaplar, `[0]` ile değil.
    /// 1200x600 desteklenen bir mod olduğundan iki eksenin birbirinden bağımsız
    /// kullanılması gerekir.
    #[test]
    fn test_page_height_lines_uses_vertical_resolution() {
        assert_eq!(points_to_pixels(842, 600), 7017);
        assert_eq!(points_to_pixels(842, 300), 3509);
        assert_eq!(points_to_pixels(842, 1200), 14034);

        // Sınır gerçekten çözünürlükle ölçekleniyor: 600 DPI'da geçerli olan
        // bir yükseklik, aynı kağıtta 300 DPI'da reddedilmeli.
        let mut h300 = valid_header();
        h300.hw_resolution = [300, 300];
        h300.width = 2480;
        h300.bytes_per_line = 310;
        h300.height = 6817; // 600 DPI'nın satır sayısı
        assert!(
            validate_page_header(&h300).is_err(),
            "300 DPI'da 600 DPI'nın satır sayısı kabul edilmemeli"
        );

        h300.height = 3408; // 300 DPI için gerçek cupsfilter değeri
        assert!(validate_page_header(&h300).is_ok());
    }

    /// Asimetrik çözünürlük (`1200x600dpi`) DESTEKLENEN gerçek bir QPDL
    /// modudur — SpliX de aynı seçeneği ml2010/ml2015/ml1640/ml2510/ml2525
    /// PPD'lerinde sunar — ve reddedilmemelidir.
    #[test]
    fn test_validate_page_header_accepts_asymmetric_resolution() {
        // A4 @ 1200x600: cupsfilter'ın gerçekte ürettiği değerler.
        let mut h = valid_header();
        h.hw_resolution = [1200, 600];
        h.width = 9517;
        h.bytes_per_line = 1190;
        h.height = 6817;
        assert!(
            validate_page_header(&h).is_ok(),
            "1200x600dpi gerçek bir QPDL modu, reddedilmemeli: {:?}",
            validate_page_header(&h).err()
        );
    }

    /// PPD'nin sunduğu her çözünürlük, filtre tarafından da kabul edilmeli:
    /// aksi hâlde kullanıcı sebebi belirsiz bir "filter failed" görür.
    #[test]
    fn test_filter_accepts_every_ppd_resolution() {
        for (x, y) in ppd_resolutions() {
            // O çözünürlükte A4 için tutarlı bir başlık kur.
            let mut h = valid_header();
            h.hw_resolution = [x, y];
            h.bytes_per_line = (595 * x).div_ceil(72).div_ceil(8);
            h.width = h.bytes_per_line * 8;
            h.height = (842 * y).div_ceil(72);
            assert!(
                validate_page_header(&h).is_ok(),
                "PPD {}x{} DPI sunuyor ama filtre reddediyor: {:?}",
                x,
                y,
                validate_page_header(&h).err()
            );
        }
    }

    /// `cupsColorOrder` denetlenmeli.
    #[test]
    fn test_validate_page_header_checks_color_order() {
        // 1-bit tek kanallı veride üç dizilim de eşdeğerdir, üçü de kabul.
        for order in [
            CupsColorOrder::Chunked,
            CupsColorOrder::Banded,
            CupsColorOrder::Planar,
        ] {
            let mut header = valid_header();
            header.color_order = order;
            assert!(
                validate_page_header(&header).is_ok(),
                "{:?} kabul edilmeliydi",
                order
            );
        }

        let mut unknown = valid_header();
        unknown.color_order = CupsColorOrder::Unknown(99);
        assert!(
            validate_page_header(&unknown).is_err(),
            "tanınmayan dizilim reddedilmeli"
        );
    }

    /// İşin sayfa sayısına filtre bir üst sınır koymaz; kota gerekiyorsa
    /// CUPS'ın kendi `job-page-limit` seçeneği kullanılır.
    #[test]
    fn test_long_jobs_are_not_capped() {
        // 1500 küçük sayfa: A6 @ 300 DPI, 8x1 piksel.
        let spec = RasterSpec {
            page_pt: (297, 420),
            width_px: 8,
            pages: 1_500,
            ..RasterSpec::a4(300, 300, 1)
        };
        let pages = parse_spl(&run_filter(spec.build()));
        assert_eq!(pages.len(), 1_500);
    }

    #[test]
    fn test_validate_page_header_rejects_oversized_fields() {
        let mut over_dpi = valid_header();
        over_dpi.hw_resolution = [9999, 9999];
        assert!(validate_page_header(&over_dpi).is_err());

        let mut over_points = valid_header();
        over_points.page_size_points = [999_999, 999_999];
        assert!(validate_page_header(&over_points).is_err());

        let mut over_lines = valid_header();
        over_lines.height = 9_999_999;
        assert!(validate_page_header(&over_lines).is_err());

        let mut over_bpl = valid_header();
        over_bpl.width = 9_999_999;
        over_bpl.bytes_per_line = 9_999_999;
        assert!(validate_page_header(&over_bpl).is_err());
    }

    // ======================================================================
    // Şerit yüksekliği: SpliX compress.cpp `_compressBandedPage`
    //   if (xResolution == 300 && yResolution == 300) bandHeight /= 2;
    // ======================================================================

    #[test]
    fn test_band_height_is_halved_only_at_300x300() {
        let with = |x, y| {
            let mut h = valid_header();
            h.hw_resolution = [x, y];
            band_height_for(&h)
        };
        assert_eq!(
            with(300, 300),
            64,
            "300x300 DPI'da şerit yüksekliği 64 olmalı"
        );
        assert_eq!(with(600, 600), QPDL_BAND_HEIGHT);
        assert_eq!(with(1200, 1200), QPDL_BAND_HEIGHT);
        // Kural İKİ eksenin de 300 olmasını istiyor; asimetrik modlar 128'de kalır.
        assert_eq!(with(1200, 600), QPDL_BAND_HEIGHT);
        assert_eq!(with(300, 600), QPDL_BAND_HEIGHT);
        assert_eq!(with(600, 300), QPDL_BAND_HEIGHT);
    }

    /// Uçtan uca: 300 DPI bir iş, tele GERÇEKTEN 64 satırlık şerit kayıtları
    /// yazmalı. Regresyon değeri buradadır — `band_height_for` doğru olsa bile
    /// çağrı yerinde kullanılmazsa bu test kırılır.
    #[test]
    fn test_300dpi_job_writes_64_line_band_records() {
        let spec = RasterSpec::a4(300, 300, 200);
        let pages = parse_spl(&run_filter(spec.build()));
        assert_eq!(pages.len(), 1);
        let bands = &pages[0].bands;
        assert_eq!(
            bands.len(),
            200_usize.div_ceil(64),
            "200 satır / 64 = 4 şerit"
        );
        for (i, b) in bands.iter().enumerate() {
            assert_eq!(b.height_lines, 64, "şerit {} yüksekliği 64 olmalı", i);
            assert_eq!(b.index, i as u8);
        }
    }

    #[test]
    fn test_600dpi_job_still_writes_128_line_band_records() {
        let spec = RasterSpec::a4(600, 600, 200);
        let pages = parse_spl(&run_filter(spec.build()));
        let bands = &pages[0].bands;
        assert_eq!(
            bands.len(),
            200_usize.div_ceil(128),
            "200 satır / 128 = 2 şerit"
        );
        assert!(bands.iter().all(|b| b.height_lines == 128));
    }

    /// Asimetrik `1200x600dpi` modu 300 DPI kuralına takılmamalı.
    #[test]
    fn test_asymmetric_1200x600_keeps_128_line_bands() {
        let spec = RasterSpec::a4(1200, 600, 200);
        let pages = parse_spl(&run_filter(spec.build()));
        assert!(pages[0].bands.iter().all(|b| b.height_lines == 128));
    }

    /// PPD'nin sunduğu HER çözünürlük için şerit yüksekliği SpliX kuralıyla
    /// aynı olmalı. PPD'ye yeni bir çözünürlük eklenirse bu test onu kapsar.
    #[test]
    fn test_band_height_matches_splix_rule_for_every_ppd_resolution() {
        for (x, y) in ppd_resolutions() {
            let expected = if x == 300 && y == 300 { 64 } else { 128 };
            let mut h = valid_header();
            h.hw_resolution = [x, y];
            assert_eq!(
                band_height_for(&h),
                expected,
                "PPD {}x{} DPI sunuyor; SpliX kuralına göre şerit yüksekliği {} olmalı",
                x,
                y,
                expected
            );
        }
    }

    // ======================================================================
    // Tek taraflı baskı (ML-2165'te dupleks yok)
    // ======================================================================

    /// Her sayfada duplex baytı 1, tumble baytı 0 (SpliX'in Simplex değerleri)
    /// ve PJL'de `DUPLEX=OFF` olmalı.
    #[test]
    fn test_simplex_pages_have_duplex_byte_one_and_no_tumble() {
        let mut spec = RasterSpec::a4(600, 600, 8);
        spec.pages = 3;
        let out = run_filter(spec.build());
        let pjl = pjl_text(&out);
        assert!(pjl.contains("@PJL SET DUPLEX=OFF\n"), "PJL: {}", pjl);
        assert!(!pjl.contains("BINDING"), "PJL: {}", pjl);
        let pages = parse_spl(&out);
        assert_eq!(pages.len(), 3);
        for (i, p) in pages.iter().enumerate() {
            assert_eq!(
                p.header[0xB],
                1,
                "sayfa {}: Simplex duplex baytı 1 olmalı",
                i + 1
            );
            assert_eq!(
                p.header[0xC],
                0,
                "sayfa {}: Simplex tumble baytı 0 olmalı",
                i + 1
            );
        }
    }

    // ======================================================================
    // Kağıt boyutu eşlemesi
    // ======================================================================

    /// PPD'nin sunduğu her kağıt boyutu, QPDL'nin doğru kağıt koduna
    /// eşlenmeli. PPD ile tablo arasındaki her sapma, meşru bir işi reddeder;
    /// bu test iki listeyi birlikte güncel tutar.
    #[test]
    fn test_every_ppd_paper_size_maps_to_its_qpdl_code() {
        use spl::SplPaperSize;

        let expected = |name: &str| -> SplPaperSize {
            match name {
                "A4" => SplPaperSize::A4,
                "Letter" => SplPaperSize::Letter,
                "Legal" => SplPaperSize::Legal,
                "Executive" => SplPaperSize::Executive,
                "A5" => SplPaperSize::A5,
                "A6" => SplPaperSize::A6,
                "B5" => SplPaperSize::B5,
                "Env10" => SplPaperSize::Env10,
                "EnvDL" => SplPaperSize::Dl,
                "EnvC5" => SplPaperSize::C5,
                "Folio" => SplPaperSize::Folio,
                other => panic!("PPD'de tabloya eklenmemiş kağıt boyutu: {}", other),
            }
        };

        let papers = ppd_paper_dimensions();
        assert_eq!(
            papers.len(),
            11,
            "PPD'den beklenen sayıda kağıt boyutu okunamadı"
        );
        for (name, w, h) in papers {
            assert_eq!(
                SplPaperSize::from_dimensions_pt_exact(w, h),
                Some(expected(&name)),
                "PPD '{}' = {}x{} pt, ama kesin eşleme başka bir kod veriyor",
                name,
                w,
                h
            );
        }
    }

    /// Folio 210x330 mm'dir (595x935 pt). 612x936 pt olan 8.5x13 inç ölçüsü
    /// Adobe adlandırmasında FanFoldGermanLegal'dir ve Folio değildir;
    /// `cupstestppd` de PPD'yi tam bu gerekçeyle uyarıyordu.
    #[test]
    fn test_folio_is_f4_not_fanfold_german_legal() {
        use spl::SplPaperSize;
        assert_eq!(
            SplPaperSize::from_dimensions_pt_exact(595, 935),
            Some(SplPaperSize::Folio)
        );
        assert_eq!(
            SplPaperSize::from_dimensions_pt_exact(935, 595),
            Some(SplPaperSize::Folio)
        );
        // 8.5x13 inç Folio'ya ya da sessizce A4'e eşlenmemeli.
        assert_eq!(SplPaperSize::from_dimensions_pt_exact(612, 936), None);
    }

    // ======================================================================
    // Kağıt kaynağı (PPD *InputSlot -> CUPS MediaPosition -> QPDL 0x9 baytı)
    // ======================================================================

    /// PPD'nin sunduğu her kağıt kaynağı, QPDL'nin doğru kaynak koduna
    /// eşlenmeli.
    ///
    /// PPD `*InputSlot` seçeneklerini doğrudan QPDL kodlarıyla
    /// numaralandırıyor (`<</MediaPosition 1>>` = Auto). PPD'ye QPDL kodu
    /// olmayan bir değer eklenirse seçenek sessizce Auto'ya düşerdi.
    #[test]
    fn test_every_ppd_input_slot_maps_to_its_qpdl_code() {
        use spl::SplPaperSource;

        let ppd = ppd_text();

        let expected = |name: &str| -> SplPaperSource {
            match name {
                "Auto" => SplPaperSource::Auto,
                "Manual" => SplPaperSource::Manual,
                other => panic!("PPD'de tabloya eklenmemiş kağıt kaynağı: {}", other),
            }
        };

        let mut checked = 0;
        for line in ppd.lines() {
            let Some(rest) = line.strip_prefix("*InputSlot ") else {
                continue;
            };
            let (name, code) = rest.split_once(':').expect("bozuk *InputSlot satırı");
            let name = name.split('/').next().unwrap().trim();
            // "<</MediaPosition 2>>setpagedevice" -> 2
            let pos: u32 = code
                .split("MediaPosition")
                .nth(1)
                .expect("MediaPosition yok")
                .trim_start()
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .parse()
                .expect("MediaPosition sayı değil");

            assert_eq!(
                SplPaperSource::from_media_position(pos),
                Some(expected(name)),
                "PPD '{}' = MediaPosition {}, ama filtre başka bir koda eşliyor",
                name,
                pos
            );
            checked += 1;
        }
        assert_eq!(
            checked, 2,
            "PPD'den beklenen sayıda kağıt kaynağı okunamadı"
        );
    }

    /// Uçtan uca: "Manual Feeder" seçimi QPDL sayfa başlığının 0x9 baytına
    /// ulaşmalı.
    #[test]
    fn test_input_slot_reaches_qpdl_page_header() {
        for (media_position, expected) in [(1u32, 1u8), (2, 2), (0, 1)] {
            let mut spec = RasterSpec::a4(600, 600, 8);
            spec.media_position = media_position;
            let pages = parse_spl(&run_filter(spec.build()));
            assert_eq!(
                pages[0].header[0x9], expected,
                "MediaPosition {} -> QPDL kaynak kodu {} olmalı",
                media_position, expected
            );
        }
    }

    /// Tanınmayan bir `MediaPosition` sessizce yanlış bir koda dönüşmemeli;
    /// Auto'ya düşmeli.
    #[test]
    fn test_unknown_media_position_falls_back_to_auto() {
        use spl::SplPaperSource;
        assert_eq!(SplPaperSource::from_media_position(6), None);
        assert_eq!(SplPaperSource::from_media_position(u32::MAX), None);

        let mut spec = RasterSpec::a4(600, 600, 8);
        spec.media_position = 6;
        let pages = parse_spl(&run_filter(spec.build()));
        assert_eq!(pages[0].header[0x9], 1, "tanınmayan kaynak Auto'ya düşmeli");
    }

    // ======================================================================
    // Kağıt türü (PPD *MediaType -> CUPS MediaType -> @PJL SET PAPERTYPE)
    // ======================================================================

    /// PPD'nin sunduğu HER kağıt türü anahtarı, filtre tarafından da
    /// tanınmalı. Aksi hâlde kullanıcının seçimi sessizce `OFF`'a düşer ve
    /// zarf/etiket düz kağıt füzer ayarlarıyla basılır.
    #[test]
    fn test_every_ppd_media_type_is_accepted_by_the_filter() {
        let ppd = ppd_text();

        let mut checked = 0;
        for line in ppd.lines() {
            let Some(rest) = line.strip_prefix("*MediaType ") else {
                continue;
            };
            let (name, code) = rest.split_once(':').expect("bozuk *MediaType satırı");
            let name = name.split('/').next().unwrap().trim();

            assert_eq!(
                spl::pjl_paper_type(name),
                Some(name),
                "PPD '{}' sunuyor ama filtrenin PJL sözlüğünde yok",
                name
            );
            // Seçim raster başlığına ulaşmalı: PostScript kodu MediaType
            // dizesini anahtarın KENDİSİYLE ayarlamalı, aksi hâlde filtre
            // farklı bir değer görür.
            assert!(
                code.contains(&format!("MediaType({})", name)),
                "PPD '{}' seçimi raster başlığına aynı anahtarla ulaşmıyor: {}",
                name,
                line
            );
            checked += 1;
        }
        assert_eq!(
            checked,
            spl::PJL_PAPER_TYPES.len(),
            "PPD, yazıcının PJL sözlüğündeki türlerin tamamını sunmuyor"
        );
    }

    /// Uçtan uca: seçilen kağıt türü PJL başlığına ulaşmalı.
    #[test]
    fn test_media_type_reaches_pjl_papertype() {
        for media_type in ["ENV", "LABEL", "THICK", "OFF"] {
            let mut spec = RasterSpec::a4(600, 600, 8);
            spec.media_type = media_type;
            let out = run_filter(spec.build());
            let pjl = pjl_text(&out);
            assert!(
                pjl.contains(&format!("@PJL SET PAPERTYPE={}\n", media_type)),
                "{} PJL'e ulaşmadı: {}",
                media_type,
                pjl
            );
        }
    }

    /// Tanınmayan ya da boş bir `MediaType` güvenli varsayılana düşmeli ve
    /// asla ham olarak PJL satırına yazılmamalı (satır tırnaksızdır: bir
    /// boşluk ya da CR/LF komutu bozardı).
    #[test]
    fn test_unknown_media_type_falls_back_to_papertype_off() {
        for media_type in ["", "Envelope", "Plain", "EVIL VALUE"] {
            let mut spec = RasterSpec::a4(600, 600, 8);
            spec.media_type = media_type;
            let out = run_filter(spec.build());
            let pjl = pjl_text(&out);
            assert!(
                pjl.contains("@PJL SET PAPERTYPE=OFF\n"),
                "{:?} için OFF'a düşülmedi: {}",
                media_type,
                pjl
            );
            assert!(
                !pjl.contains("PAPERTYPE=EVIL"),
                "güvenilmez değer PJL satırına sızdı: {}",
                pjl
            );
        }
    }

    /// PPD varsayılanları filtrenin varsayılanlarıyla uyuşmalı: PPD
    /// `*DefaultMediaType: OFF` / `*DefaultInputSlot: Auto` diyorsa, hiçbir
    /// seçim yapılmamış bir iş de aynı sonucu üretmeli.
    #[test]
    fn test_ppd_defaults_match_filter_defaults() {
        let ppd = ppd_text();

        let default_of = |key: &str| -> String {
            ppd.lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("{} yok", key))
                .trim()
                .to_string()
        };
        assert_eq!(default_of("*DefaultMediaType:"), spl::PJL_PAPERTYPE_DEFAULT);
        assert_eq!(default_of("*DefaultInputSlot:"), "Auto");
    }
}
