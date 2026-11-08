//! # CUPS Raster Parser
//!
//! cups-filters zincirinden gelen klasik CUPS Raster akışını (`RaSt`, `RaS2`,
//! `RaS3`; `cups_page_header2_t`) ayrıştırır.
//!
//! v1 ve v3 sayfa verisini sıkıştırmasız taşır; v2 (`RaS2`/`2SaR`, yani PWG
//! Raster) satır-RLE kullanır ve `CupsLineDecoder` tarafından şeffaf biçimde
//! çözülür. Çağıran her durumda `CupsRasterReader::read_line` kullanır.

use std::fmt;
use std::io::{self, Read};

/// CUPS Raster senkronizasyon (magic) sözcükleri.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CupsRasterVersion {
    /// CUPS Raster Sürüm 1 - Big Endian (`RaSt`)
    V1Be,
    /// CUPS Raster Sürüm 1 - Little Endian (`tSaR`)
    V1Le,
    /// CUPS Raster Sürüm 2 - Big Endian (`RaS2`)
    V2Be,
    /// CUPS Raster Sürüm 2 - Little Endian (`2SaR`)
    V2Le,
    /// CUPS Raster Sürüm 3 - Big Endian (`RaS3`)
    V3Be,
    /// CUPS Raster Sürüm 3 - Little Endian (`3SaR`)
    V3Le,
}

impl CupsRasterVersion {
    #[inline]
    pub fn is_big_endian(&self) -> bool {
        matches!(
            self,
            CupsRasterVersion::V1Be | CupsRasterVersion::V2Be | CupsRasterVersion::V3Be
        )
    }

    /// Sayfa başlığının bayt boyutu, CUPS'un `<cups/raster.h>` yapılarına
    /// karşı `sizeof` ile ölçüldü: v1 = 420 (`cups_page_header_t`, son alan
    /// `cupsRowStep` 416..420), v2/v3 = 1796 (`cups_page_header2_t`). Yanlış
    /// bir değer her sayfada piksel verisinden bayt yutar ve sonraki başlığı
    /// kaydırır.
    #[inline]
    pub fn header_size(&self) -> usize {
        match self {
            CupsRasterVersion::V1Be | CupsRasterVersion::V1Le => 420,
            _ => 1796,
        }
    }

    /// Sayfa verisi CUPS satır-RLE'si ile sıkıştırılmış mı? Yalnızca v2:
    /// `<cups/raster.h>`'te `CUPS_RASTER_SYNC_PWG == CUPS_RASTER_SYNCv2` ve PWG
    /// Raster tanımı gereği sıkıştırılmıştır.
    #[inline]
    pub fn is_compressed(&self) -> bool {
        matches!(self, CupsRasterVersion::V2Be | CupsRasterVersion::V2Le)
    }
}

/// CUPS renk uzayı (`cups_cspace_e`), ham sayısal kod. Sürücü yalnızca `K`
/// ile çalışır ve diğer her değeri reddeder; adlar yalnızca hata mesajlarında
/// görünür.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CupsColorSpace(pub u32);

impl CupsColorSpace {
    /// Siyah-tonlama (0 = beyaz, 1 = siyah). Lazer motorunun beklediği tek uzay.
    pub const K: CupsColorSpace = CupsColorSpace(3);
}

impl fmt::Display for CupsColorSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.0 {
            0 => "W (White=0 Grayscale)",
            1 => "RGB",
            2 => "RGBA",
            3 => "K (Black=0 Grayscale)",
            4 => "CMY",
            5 => "CMYK",
            18 => "sGray (sRGB Grayscale)",
            19 => "sRGB",
            20 => "AdobeRGB",
            other => return write!(f, "Bilinmeyen({})", other),
        };
        write!(f, "{}", name)
    }
}

/// CUPS renk dizilimi (`cups_order_e`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CupsColorOrder {
    Chunked,
    Banded,
    Planar,
    Unknown(u32),
}

impl From<u32> for CupsColorOrder {
    fn from(val: u32) -> Self {
        match val {
            0 => CupsColorOrder::Chunked,
            1 => CupsColorOrder::Banded,
            2 => CupsColorOrder::Planar,
            other => CupsColorOrder::Unknown(other),
        }
    }
}

/// CUPS sayfa başlığının bu filtrenin kullandığı alanları. Ofsetler
/// `cups_page_header_t` / `cups_page_header2_t` yapısındaki konumlardır.
#[derive(Debug, Clone)]
pub struct PageHeader {
    /// `MediaType` (128..192) — PPD `*MediaType`, PJL `PAPERTYPE`'a eşlenir.
    pub media_type: String,
    /// `HWResolution` (276..284) — [X DPI, Y DPI]
    pub hw_resolution: [u32; 2],
    /// `Margins` (312..320) — [sol, alt] pt; PPD `*ImageableArea`'dan.
    pub margins: [u32; 2],
    /// `MediaPosition` (324) — PPD `*InputSlot`, QPDL kağıt kaynağı kodu.
    pub media_position: u32,
    /// `NumCopies` (340)
    pub num_copies: u32,
    /// `PageSize` (352..360) — [genişlik, uzunluk] pt
    pub page_size_points: [u32; 2],
    /// `cupsWidth` (372), piksel
    pub width: u32,
    /// `cupsHeight` (376), satır
    pub height: u32,
    /// `cupsBitsPerColor` (384)
    pub bits_per_color: u32,
    /// `cupsBitsPerPixel` (388)
    pub bits_per_pixel: u32,
    /// `cupsBytesPerLine` (392)
    pub bytes_per_line: u32,
    /// `cupsColorOrder` (396)
    pub color_order: CupsColorOrder,
    /// `cupsColorSpace` (400)
    pub color_space: CupsColorSpace,
    /// `cupsCompression` (404) — sürücüye özel ipucu, akış sıkıştırması değil.
    pub compression: u32,
    /// `cupsPageSizeName` (1732..1796, yalnızca v2/v3) — teşhis için.
    pub page_size_name: Option<String>,
}

impl PageHeader {
    fn parse_c_string(bytes: &[u8]) -> String {
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..len]).trim().to_string()
    }

    #[inline]
    fn read_u32(buf: &[u8], offset: usize, is_be: bool) -> u32 {
        let slice: [u8; 4] = buf[offset..offset + 4]
            .try_into()
            .expect("Hatalı dilim uzunluğu");
        if is_be {
            u32::from_be_bytes(slice)
        } else {
            u32::from_le_bytes(slice)
        }
    }

    /// CUPS Raster sayfa başlığını ayrıştırır.
    pub fn parse(buf: &[u8], version: CupsRasterVersion) -> io::Result<Self> {
        let is_be = version.is_big_endian();
        let expected_size = version.header_size();

        if buf.len() < expected_size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "CUPS Raster başlık boyutu yetersiz (beklenen: {} bayt, gelen: {} bayt)",
                    expected_size,
                    buf.len()
                ),
            ));
        }

        let u32_at = |offset: usize| Self::read_u32(buf, offset, is_be);

        let page_size_name = if expected_size >= 1796 {
            Some(Self::parse_c_string(&buf[1732..1796])).filter(|name| !name.is_empty())
        } else {
            None
        };

        Ok(Self {
            media_type: Self::parse_c_string(&buf[128..192]),
            hw_resolution: [u32_at(276), u32_at(280)],
            margins: [u32_at(312), u32_at(316)],
            media_position: u32_at(324),
            num_copies: u32_at(340),
            page_size_points: [u32_at(352), u32_at(356)],
            width: u32_at(372),
            height: u32_at(376),
            bits_per_color: u32_at(384),
            bits_per_pixel: u32_at(388),
            bytes_per_line: u32_at(392),
            color_order: CupsColorOrder::from(u32_at(396)),
            color_space: CupsColorSpace(u32_at(400)),
            compression: u32_at(404),
            page_size_name,
        })
    }

    /// Sayfaya ait ham (sıkıştırılmamış) toplam piksel verisi boyutu.
    #[inline]
    pub fn total_raster_bytes(&self) -> u64 {
        (self.bytes_per_line as u64) * (self.height as u64)
    }
}

/// Akıştan CUPS Raster verisini okuyan ayrıştırıcı.
pub struct CupsRasterReader<R: Read> {
    reader: R,
    version: CupsRasterVersion,
    /// v2 akışlarında satır çözücüsünün sayfa başına durumu; v1/v3'te `None`.
    decoder: Option<CupsLineDecoder>,
}

impl<R: Read> CupsRasterReader<R> {
    /// 4 baytlık senkronizasyon sözcüğünü doğrulayarak okuyucuyu başlatır.
    pub fn new(mut reader: R) -> io::Result<Self> {
        let mut magic = [0u8; 4];
        if let Err(e) = reader.read_exact(&mut magic) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Girdi akışı boş (0 bayt). cupsfilter veya önceki filtre aşamasının başarıyla raster ürettiğinden emin olun.",
                ));
            }
            return Err(e);
        }

        let version = match &magic {
            b"RaSt" => CupsRasterVersion::V1Be,
            b"tSaR" => CupsRasterVersion::V1Le,
            b"RaS2" => CupsRasterVersion::V2Be,
            b"2SaR" => CupsRasterVersion::V2Le,
            b"RaS3" => CupsRasterVersion::V3Be,
            b"3SaR" => CupsRasterVersion::V3Le,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Geçersiz CUPS Raster formatı: {:?} (Beklenen: 'RaSt', 'tSaR', 'RaS2', '2SaR', 'RaS3', '3SaR')",
                        String::from_utf8_lossy(other)
                    ),
                ));
            }
        };

        Ok(Self {
            reader,
            version,
            decoder: None,
        })
    }

    #[inline]
    pub fn version(&self) -> CupsRasterVersion {
        self.version
    }

    /// Sonraki sayfa başlığını okur. Akış iki sayfa arasında temiz biterse
    /// (0 bayt) `Ok(None)` döner.
    ///
    /// `read_exact` başlık ortasında kesilmiş bir akışı normal akış sonundan
    /// ayırt edemez (ikisi de `UnexpectedEof`); baytlar bu yüzden sayılarak
    /// okunur, böylece bozulma "iş bitti" sanılmaz.
    pub fn next_page_header(&mut self) -> io::Result<Option<PageHeader>> {
        let header_len = self.version.header_size();
        let mut buf = vec![0u8; header_len];

        let mut total_read = 0usize;
        while total_read < header_len {
            match self.reader.read(&mut buf[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }

        if total_read == 0 {
            return Ok(None);
        }
        if total_read < header_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "CUPS Raster akışı sayfa başlığının ortasında kesildi \
                     ({} / {} bayt okundu). Önceki filtre aşaması yarıda kesilmiş olabilir.",
                    total_read, header_len
                ),
            ));
        }

        let header = PageHeader::parse(&buf, self.version)?;

        // Çözücü durumu sayfa başına sıfırlanır: satır tekrar sayacı sonraki
        // sayfaya sızmamalı.
        self.decoder = self.version.is_compressed().then(CupsLineDecoder::default);

        Ok(Some(header))
    }

    /// Sayfa verisinden TEK BİR SATIR okur ve `out`'u tamamen doldurur.
    ///
    /// `out`'un uzunluğu `cupsBytesPerLine` olmalıdır. v2 çözücüsü tamponunu
    /// bu uzunluktan kurar, başlıktaki henüz doğrulanmamış alandan değil;
    /// böylece bozuk bir başlık doğrulamadan önce devasa bir tahsis yaptıramaz.
    pub fn read_line(&mut self, out: &mut [u8]) -> io::Result<()> {
        match &mut self.decoder {
            None => self.reader.read_exact(out),
            Some(decoder) => decoder.read_line(&mut self.reader, out),
        }
    }
}

/// CUPS Raster v2 (`RaS2`/`2SaR`, PWG Raster ile aynı) satır-RLE çözücüsü.
///
/// ```text
/// [tekrar]  : bu satır (tekrar + 1) kez yinelenir
/// ardından cupsBytesPerLine dolana kadar:
///   n == 128 : satırın sonuna kadar boş renkle doldur
///   n >  128 : (257 - n) pikselin ham (literal) kopyası gelir
///   n <  128 : sonraki tek piksel (n + 1) kez yinelenir
/// ```
///
/// Sayaçlar PİKSEL cinsindendir. Filtre yalnızca 1-bit K kabul ettiği (bkz.
/// `validate_page_header`) ve satırlar ancak doğrulamadan sonra okunduğu için
/// piksel 1 bayt, boş renk de K'nin boşluğu olan 0x00'dır (libcups
/// `cups_raster_update`). Kodlama libcups'un `cupsRasterWritePixels`
/// çıktısına karşı doğrulandı: 620 bayt sıfırdan oluşan 200 satırlık bir sayfa
/// `c7 7f 00 7f 00 7f 00 7f 00 6b 00` olarak kodlanır.
#[derive(Default)]
struct CupsLineDecoder {
    /// Son çözülen satırın kaç kez daha tekrarlanacağı.
    repeat_remaining: u32,
    /// Son çözülen satır; tekrarlar buradan kopyalanır. İlk `read_line`
    /// çağrısında çağıranın tampon uzunluğundan boyutlandırılır.
    last_line: Vec<u8>,
}

impl CupsLineDecoder {
    /// K renk uzayında boşluk: toner yok.
    const BLANK_FILL: u8 = 0x00;

    fn read_line<R: Read>(&mut self, reader: &mut R, out: &mut [u8]) -> io::Result<()> {
        if out.is_empty() {
            return Ok(());
        }

        // Satır uzunluğu sayfa ortasında değişemez; değişirse çağıranın
        // hatasıdır ve sessizce yanlış çözmektense hata verilir.
        if self.last_line.is_empty() {
            self.last_line = vec![0u8; out.len()];
        } else if self.last_line.len() != out.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Satır uzunluğu sayfa ortasında değişti: {} -> {}",
                    self.last_line.len(),
                    out.len()
                ),
            ));
        }

        if self.repeat_remaining > 0 {
            self.repeat_remaining -= 1;
        } else {
            self.repeat_remaining = read_u8(reader)? as u32;
            self.decode_line(reader)?;
        }
        out.copy_from_slice(&self.last_line);
        Ok(())
    }

    fn decode_line<R: Read>(&mut self, reader: &mut R) -> io::Result<()> {
        let line_len = self.last_line.len();
        let mut pos = 0usize;

        while pos < line_len {
            let n = read_u8(reader)?;

            if n == 128 {
                self.last_line[pos..].fill(Self::BLANK_FILL);
                return Ok(());
            }

            // n > 128: (257 - n) ham bayt (n=255 -> 2, n=129 -> 128);
            // n < 128: sonraki bayt (n + 1) kez.
            let (count, kind) = if n > 128 {
                (257 - n as usize, "literal kayıt")
            } else {
                (n as usize + 1, "tekrar kaydı")
            };
            // Satırı aşan sayaç kırpılmaz: kırpmak, sonraki kaydın baytlarını
            // kontrol baytı sandırıp akışın geri kalanını kaydırırdı.
            if count > line_len - pos {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "CUPS v2 {} satır sınırını aşıyor: kalan {} bayt, kayıt {} bayt",
                        kind,
                        line_len - pos,
                        count
                    ),
                ));
            }

            let run = &mut self.last_line[pos..pos + count];
            if n > 128 {
                reader.read_exact(run)?;
            } else {
                run.fill(read_u8(reader)?);
            }
            pos += count;
        }

        Ok(())
    }
}

#[inline]
fn read_u8<R: Read>(reader: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    reader.read_exact(&mut b)?;
    Ok(b[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_cups_sync_words() {
        // Altı sürüm de kabul edilir; v2 (`RaS2`/`2SaR`) sayfa verisini
        // satır-RLE ile taşır ve `CupsLineDecoder` tarafından şeffaf biçimde
        // çözülür (bkz. main.rs `test_v2_and_v3_streams_produce_identical_output`).
        // Burada temsilci olarak bir v3 ve bir v1 akışı üzerinden sürüm/endian
        // eşlemesini doğruluyoruz, v2'nin bayrakları ise aşağıda ayrıca.
        let v3_be = b"RaS3";
        let reader = CupsRasterReader::new(Cursor::new(v3_be)).unwrap();
        assert_eq!(reader.version(), CupsRasterVersion::V3Be);
        assert!(reader.version().is_big_endian());
        assert_eq!(reader.version().header_size(), 1796);

        let v1_le = b"tSaR";
        let reader_v1 = CupsRasterReader::new(Cursor::new(v1_le)).unwrap();
        assert_eq!(reader_v1.version(), CupsRasterVersion::V1Le);
        assert!(!reader_v1.version().is_big_endian());
        assert_eq!(reader_v1.version().header_size(), 420);

        // Sürüm eşlemesi, akışı reddetmekten bağımsız olarak doğru kalmalı.
        assert_eq!(CupsRasterVersion::V2Be.header_size(), 1796);
        assert!(CupsRasterVersion::V2Be.is_compressed());
        assert!(CupsRasterVersion::V2Le.is_compressed());
    }

    #[test]
    fn test_cups_header_parse_canonical() {
        let mut header_buf = vec![0u8; 1796];

        // HWResolution: [600, 600]
        header_buf[276..280].copy_from_slice(&600u32.to_be_bytes());
        header_buf[280..284].copy_from_slice(&600u32.to_be_bytes());

        // PageSize points: [595, 842] (A4)
        header_buf[352..356].copy_from_slice(&595u32.to_be_bytes());
        header_buf[356..360].copy_from_slice(&842u32.to_be_bytes());

        // width: 4960, height: 7016
        header_buf[372..376].copy_from_slice(&4960u32.to_be_bytes());
        header_buf[376..380].copy_from_slice(&7016u32.to_be_bytes());

        // bits_per_color: 1, bits_per_pixel: 1, bytes_per_line: 620
        header_buf[384..388].copy_from_slice(&1u32.to_be_bytes());
        header_buf[388..392].copy_from_slice(&1u32.to_be_bytes());
        header_buf[392..396].copy_from_slice(&620u32.to_be_bytes());

        // color_space: 3 (K/Black)
        header_buf[400..404].copy_from_slice(&3u32.to_be_bytes());

        // cupsPageSizeName at offset 1732: "A4"
        header_buf[1732..1734].copy_from_slice(b"A4");

        let header = PageHeader::parse(&header_buf, CupsRasterVersion::V2Be).unwrap();
        assert_eq!(header.hw_resolution, [600, 600]);
        assert_eq!(header.page_size_points, [595, 842]);
        assert_eq!(header.width, 4960);
        assert_eq!(header.height, 7016);
        assert_eq!(header.bits_per_color, 1);
        assert_eq!(header.bits_per_pixel, 1);
        assert_eq!(header.bytes_per_line, 620);
        assert_eq!(header.color_space, CupsColorSpace::K);
        assert_eq!(header.page_size_name.as_deref(), Some("A4"));
        assert_eq!(header.total_raster_bytes(), 620 * 7016);
    }

    /// V1 başlık boyutu, CUPS'un `cups_page_header_t` yapısının gerçek
    /// boyutuyla (420 bayt) eşleşmeli.
    #[test]
    fn test_v1_header_size_matches_cups_struct() {
        assert_eq!(CupsRasterVersion::V1Be.header_size(), 420);
        assert_eq!(CupsRasterVersion::V1Le.header_size(), 420);
        assert_eq!(CupsRasterVersion::V3Be.header_size(), 1796);
    }

    /// Ardışık V1 sayfaları akışta senkron kalmalı: başlık boyutu bir bayt
    /// bile yanlışsa 2. sayfanın alanları kaymış ofsetten ayrıştırılır.
    #[test]
    fn test_v1_stream_stays_in_sync_across_pages() {
        const V1_HEADER_LEN: usize = 420;

        let mut header = vec![0u8; V1_HEADER_LEN];
        let mut put = |off: usize, val: u32| {
            header[off..off + 4].copy_from_slice(&val.to_be_bytes());
        };
        put(276, 600); // hw_resolution[0]
        put(280, 600); // hw_resolution[1]
        put(352, 595); // page_size_points[0]
        put(356, 842); // page_size_points[1]
        put(372, 32); // width
        put(376, 3); // height
        put(384, 1); // bits_per_color
        put(388, 1); // bits_per_pixel
        put(392, 4); // bytes_per_line = ceil(32 * 1 / 8)
        put(400, 3); // color_space = K

        let pixels = vec![0u8; 4 * 3];
        let mut stream = b"RaSt".to_vec();
        for _ in 0..2 {
            stream.extend_from_slice(&header);
            stream.extend_from_slice(&pixels);
        }

        let mut reader = CupsRasterReader::new(Cursor::new(stream)).unwrap();
        for page in 1..=2 {
            let h = reader
                .next_page_header()
                .unwrap()
                .unwrap_or_else(|| panic!("sayfa {} başlığı okunamadı", page));
            assert_eq!(h.bytes_per_line, 4, "sayfa {} kaymış", page);
            assert_eq!(h.width, 32, "sayfa {} kaymış", page);
            assert_eq!(h.height, 3, "sayfa {} kaymış", page);

            // Sayfa verisini tüket ki sıradaki başlık doğru ofsetten okunsun.
            let mut line = vec![0u8; 4];
            for _ in 0..3 {
                reader.read_line(&mut line).unwrap();
            }
        }
        assert!(
            reader.next_page_header().unwrap().is_none(),
            "akış temiz bitmeli"
        );
    }

    /// Altı sync sözcüğü de kabul edilmeli; sıkıştırma bayrağı doğru kurulmalı.
    #[test]
    fn test_accepts_all_known_sync_words() {
        for (magic, compressed) in [
            (b"RaSt", false),
            (b"tSaR", false),
            (b"RaS2", true),
            (b"2SaR", true),
            (b"RaS3", false),
            (b"3SaR", false),
        ] {
            let reader = CupsRasterReader::new(Cursor::new(magic))
                .unwrap_or_else(|e| panic!("{:?} reddedildi: {}", magic, e));
            assert_eq!(reader.version().is_compressed(), compressed, "{:?}", magic);
        }
    }

    /// 1 bayt/piksel için tek satırlık bir v2 akışı kurar.
    fn v2_page(bytes_per_line: u32, height: u32, payload: &[u8]) -> Vec<u8> {
        let mut hdr = vec![0u8; 1796];
        let mut put = |off: usize, val: u32| {
            hdr[off..off + 4].copy_from_slice(&val.to_be_bytes());
        };
        put(276, 600);
        put(280, 600);
        put(352, 595);
        put(356, 842);
        put(372, bytes_per_line * 8);
        put(376, height);
        put(384, 1);
        put(388, 1);
        put(392, bytes_per_line);
        put(400, 3); // K

        let mut stream = b"RaS2".to_vec();
        stream.extend_from_slice(&hdr);
        stream.extend_from_slice(payload);
        stream
    }

    fn decode_v2(bytes_per_line: u32, height: u32, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut reader =
            CupsRasterReader::new(Cursor::new(v2_page(bytes_per_line, height, payload))).unwrap();
        reader.next_page_header().unwrap().unwrap();
        (0..height)
            .map(|_| {
                let mut line = vec![0u8; bytes_per_line as usize];
                reader.read_line(&mut line).unwrap();
                line
            })
            .collect()
    }

    /// libcups'un ürettiği GERÇEK bayt dizisi çözülebilmeli.
    ///
    /// `cupsRasterWritePixels` ile yazılan, 620 bayt sıfırdan oluşan 200
    /// satırlık bir sayfanın tamamı tam olarak bu 11 bayttır; kodlamayı
    /// bu dosyadan bire bir okudum.
    #[test]
    fn test_v2_decodes_real_libcups_payload() {
        let payload = [
            0xc7, 0x7f, 0x00, 0x7f, 0x00, 0x7f, 0x00, 0x7f, 0x00, 0x6b, 0x00,
        ];
        let lines = decode_v2(620, 200, &payload);
        assert_eq!(lines.len(), 200);
        for (i, line) in lines.iter().enumerate() {
            assert!(line.iter().all(|&b| b == 0), "satır {} sıfır değil", i);
        }
    }

    /// Üç kayıt türü de doğru çözülmeli: tekrar, ham kopya, satır sonuna
    /// kadar boşaltma.
    #[test]
    fn test_v2_decodes_each_record_kind() {
        // [0] satır tekrarı yok
        // [2, 0xAB]      -> 0xAB x3
        // [0xFE, 1, 2, 3] -> (257-254)=3 ham bayt
        // [128]          -> satır sonuna kadar boş (K => 0x00)
        let payload = [0x00, 0x02, 0xAB, 0xFE, 0x01, 0x02, 0x03, 0x80];
        let lines = decode_v2(8, 1, &payload);
        assert_eq!(lines[0], vec![0xAB, 0xAB, 0xAB, 1, 2, 3, 0x00, 0x00]);
    }

    /// Satır tekrar sayacı: `[n]` başlığı satırı (n + 1) kez üretir.
    #[test]
    fn test_v2_line_repeat_count() {
        // [3] -> 4 satır; her satır [0,0xF0] + [128] ile 0xF0 sonra sıfırlar
        let payload = [0x03, 0x00, 0xF0, 0x80];
        let lines = decode_v2(4, 4, &payload);
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(line, &vec![0xF0, 0x00, 0x00, 0x00]);
        }
    }

    /// Satır tekrarı SAYFA sınırını aşmamalı: bir sayfadan artakalan sayaç
    /// sonraki sayfanın ilk satırına sızarsa tüm akış kayar.
    #[test]
    fn test_v2_repeat_state_resets_between_pages() {
        // Her sayfa "10 satır tekrarı" bildiriyor ama sayfa yüksekliği 1.
        let payload = [0x09, 0x00, 0xAA, 0x80];
        let mut stream = v2_page(4, 1, &payload);
        let second = v2_page(4, 1, &[0x09, 0x00, 0xBB, 0x80]);
        stream.extend_from_slice(&second[4..]); // magic'i atla

        let mut reader = CupsRasterReader::new(Cursor::new(stream)).unwrap();
        let mut line = vec![0u8; 4];

        reader.next_page_header().unwrap().unwrap();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line[0], 0xAA);

        reader.next_page_header().unwrap().unwrap();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line[0], 0xBB, "önceki sayfanın tekrar sayacı sızdı");
    }

    /// Bildirilen satırı aşan sayaçlar kırpılmamalı: kırpma, bir sonraki
    /// kaydın baytlarını kontrol baytı sanıp bütün akışın kaymasına yol açar.
    #[test]
    fn test_v2_oversized_counts_are_rejected() {
        for payload in [
            vec![0x00, 0x7F, 0x5A],
            vec![0x00, 0x81, 0x11, 0x22, 0x33, 0x44],
        ] {
            let mut reader = CupsRasterReader::new(Cursor::new(v2_page(4, 1, &payload))).unwrap();
            reader.next_page_header().unwrap().unwrap();

            let mut line = vec![0u8; 4];
            let err = reader
                .read_line(&mut line)
                .expect_err("satırı aşan v2 kaydı reddedilmeliydi");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains("satır sınırını"), "{}", err);
        }
    }

    /// Yarıda kesilen bir v2 akışı hata döndürmeli, panic atmamalı.
    #[test]
    fn test_v2_truncated_payload_errors_cleanly() {
        let mut reader =
            CupsRasterReader::new(Cursor::new(v2_page(620, 10, &[0x00, 0x7F]))).unwrap();
        reader.next_page_header().unwrap().unwrap();
        let mut line = vec![0u8; 620];
        assert!(reader.read_line(&mut line).is_err());
    }
}
