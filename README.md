# pretrain-scraper

Sıfırdan Rust ile yazılmış, **sabit bellekli**, **kesintiye dayanıklı** bir web crawler ve veri temizleme motoru. Çıktı, doğrudan LLM pretraining eğitimine verilebilecek şekilde tekilleştirilmiş, filtrelenmiş, PII maskelenmiş `JSONL + Zstandard` shard'ları ve bunların SHA-256 manifestidir.

Harici Python/JS betiği, servis veya veritabanı sunucusu gerekmez: tek bir statik binary, gömülü LSM-tree veritabanı ve disk üzerindeki shard'lar yeterlidir.

---

## 1. Ne yapar?

```
[Kalıcı Frontier (LSM DB)]
        │  bounded mpsc
        ▼
[Stage 1  Fetch + Politeness] ──keşfedilen linkler──► [Normalizer & Frontier]
        │  bounded mpsc
        ▼
[Stage 2  DOM Budama + İçerik Yoğunluğu + Markdown]
        │  bounded mpsc
        ▼
[Stage 3  FineWeb/C4/Gopher Kalite + Dil + PII]
        │  bounded mpsc
        ▼
[Stage 4  Exact XXH3-128 + SimHash/LSH Yakın Kopya]
        │  bounded mpsc
        ▼
[Stage 5  Zstd JSONL Shard + Atomik manifest.json]
```

* **Kalite**: FineWeb / RefinedWeb / Gopher / C4 sezgileri (uzunluk, ortalama kelime uzunluğu, noktalama, alfabetik kelime oranı, sembol yoğunluğu, satır/n-gram tekrarı, dil güveni, şablon sayfa ifadeleri).
* **Gürültü temizliği**: `nav/footer/header/aside/form/script/style/...` etiketleri ile `cookie|consent|gdpr|sidebar|comment|disqus|newsletter|advert|related|paywall|...` desenli kapsayıcılar budanır; kalan bloklar metin/link yoğunluğuna göre puanlanır ve en "sıkı" içerik kapsayıcısı seçilir.
* **Yapı korunur**: başlıklar `#..######`, paragraflar boş satır, listeler `-`, kod blokları ``` fence (dil etiketiyle), tablolar `a | b` satırları.
* **Tekilleştirme**: kalıcı XXH3-128 exact indeksi + 64-bit SimHash ve 4 bantlı LSH indeksi (Hamming ≤ 3). Ayrıca şablon paragraflar (gazete alt bilgisi vb.) 3'ten fazla tekrarlandığında kırpılır. Yazma sırasında korpus geneli paragraf/cümle/blok tekilleştirmesi uygulanır; `verify` yineleme sayısını raporlar ve eşiği aşarsa hata verir.
* **PII**: e-posta, Luhn doğrulamalı kredi kartı, IPv4 ve telefon numaraları maskelenir.
* **Dayanıklılık**: `SIGINT`/`SIGTERM` yakalanır, in-flight işler bitirilir, Zstd akışları `finish()` ile kapatılır, DB flush edilir ve manifest atomik olarak yazılır. Yeniden başlatıldığında frontier, visited seti ve dedup indeksleri kaldığı yerden devam eder.
* **Sabit bellek**: tüm aşamalar arası kanallar bounded; her stage'de eşzamanlı görev sayısı semaphore ile sınırlı; gövdeler streaming okunur ve `max_response_bytes` aşıldığı anda bağlantı kesilir.

---

## 2. Kurulum

```bash
# Rust 1.80+ (stable)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

git clone <repo> && cd pretrain-scraper
cargo build --release        # binary: target/release/pretrain-scraper

cp config.example.toml config.toml
# Site listesi: her satır bir site; aynı zamanda tarama kapsamıdır.
printf 'https://example.com/\nhttps://blog.example.org/\n' > sites.txt
```

Doğrulama:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --all-targets
```

---

## 3. CLI

```bash
# 1) Tarama (yeniden başlatınca kaldığı yerden devam eder)
#    Siteler `frontier.allowed_domains_file` dosyasından okunur; --seeds yoksa
#    seed'ler de oradan gelir.
pretrain-scraper crawl --config config.toml
pretrain-scraper crawl --config config.toml --max-pages 5000
#    Alternatif olarak tek seferlik seed dosyası verilebilir:
pretrain-scraper crawl --config config.toml --seeds seeds.txt
```

Depoda hazır gelen `sites_tr.txt`, robots.txt'i izin veren **130 Türkçe site** içerir (26 Kasım 2026 tarihinde kontrol edildi). `config.tr.toml` bu dosyayı `frontier.allowed_domains_file` ile okur: dosya hem tarama kapsamını hem de seed listesini verir, ayrıca `--seeds` gerekmez.

`frontier.discover_sitemaps = false` (Türkçe yapılandırmada varsayılan) ilk isteği geciktirmemek için robots.txt sitemap keşfini kapatır; linkler derinlikle bulunur. Tek bir siteyi derinlemesine tarayacaksanız açabilirsiniz.

```bash

# 2) Diskteki HTML/JSONL dosyalarını ağa çıkmadan temizle
pretrain-scraper clean-html \
  --config config.toml \
  --input-dir ./raw_html \
  --output-dir ./dataset

# 3) Frontier + dataset durumu
pretrain-scraper stats --config config.toml --output-dir ./dataset

# 4) Bütünlük doğrulama (SHA-256 + Zstd + JSON şeması + sayaçlar)
pretrain-scraper verify --output-dir ./dataset

# 5) Düz metin pretraining külliyatı (tek .txt, <veribitisi> ayırıcılı)
pretrain-scraper export-txt --dataset ./dataset --output ./dataset/corpus.txt
pretrain-scraper export-txt --dataset ./dataset --separator "<veribitisi>" --no-title
```

`clean-html` girdi biçimleri:

| Uzantı | Davranış |
|---|---|
| `.html`, `.htm`, `.xhtml` | Dosya tek belge olarak işlenir (`file://` URL'i ile) |
| `.jsonl` | Her satır `{"url": "...", "html": "..."}` (`body`/`content` alias'ları kabul edilir) |
| `.jsonl.zst` | Aynı şema, Zstandard sıkıştırılmış |

Çıkış kodları: `0` başarı, `1` hata/doğrulama problemi.

---

## 4. Çıktı formatı

Varsayılan format **düz metin**tir: `output_dir/corpus_00000.txt`, `corpus_00001.txt`, ... (varsayılan 256 MiB boyutta rotasyon). Her shard doğrudan eğitime verilebilecek bir dilimdir; her **konu kaydı** `<veribitisi>` ile ayrılır (bir sayfa birden çok konu kaydı üretebilir; `manifest.documents` bu kayıtları sayar):

```text
Türkiye - Vikipedi

Türkiye, resmî adıyla Türkiye Cumhuriyeti, topraklarının büyük bölümü Batı Asya'da ...
<veribitisi>

Genocide Watch Türkiye raporu: "Ayrımcılık, kutuplaştırma ve inkâr"

Soykırımın önlenmesi amacıyla çalışan uluslararası sivil toplum kuruluşu ...
<veribitisi>
```

* **Konu bölütleme**: her sayfa başlık hiyerarşisine (`#`, `##`, ...) göre konu bölümlerine ayrılır. `max_chunk_words` (varsayılan 3000) sınırını aşan bölüm paragraf/cümle sınırından bölünür; `min_chunk_words` (varsayılan 25) altındaki küçük bölümler sonraki bölümle birleştirilir (tek başına anlamsız parçacık üretilmez). Her bölüm kendi `<veribitisi>` kaydıdır, böylece farklı konular aynı eğitim kaydında karışmaz.
* **Başlık bir kez**: sayfa başlığı belgenin yalnızca **ilk** kaydına yazılır; sonraki konu kayıtlarına tekrarlanmaz (başlık tekrarı en büyük yapay yineleme kaynağıydı). Başlıkla aynı olan bölüm başlığı/paragrafı da düşürülür.
* **Kaydedilen sayfa defteri**: kabul edilip shard'a yazılan her belgenin tam URL'si `output.saved_urls_file` (`crawled_urls.txt`) dosyasına eklenir. Sonraki taramalar bu dosyayı yükleyip aynı sayfaları **istek atmadan** atlar; taze bir veritabanında bile tekrar tarama olmaz. Dosyaya yalnızca alt içerik sayfaları girer: anasayfa (`/`), bölüm/kategori sayfaları (`/ekonomi`, `/kategori/...`), etiket/arşiv/arama listeleri, sayfalama (`?sayfa=2`) ve `index.html` gibi giriş URL'leri elenir.
* **Korpus geneli tekilleştirme**: paragraf, cümle ve blok düzeyinde kanonik hash (küçük harf + noktalama temizliği) ile daha önce yazılmış metin atılır. Tekrarlanan manşetler, gövdede yeniden anlatılan ara paragraflar, iki kez aktarılan alıntılar ve yeniden yayımlanan metin böylece korpusa iki kez girmez. İndeks sabit kapasitelidir (`output.dedup_index_capacity`), dolunca sıfırlanır; bellek sınırlı kalır.
* **Saf metin**: Markdown başlık (`#`), liste (`-`), kod fence, tablo satırı (`a | b`), alıntı (`>`) ve vurgu (`*`, backtick) işaretleri diske yazılmadan temizlenir. Kod bloklarının **içeriği** metin olarak korunur. Metin içi sayısal atıflar (`[6]`, `[ 6 ]`), tek başına tarih satırları (`21 Haziran 2017`), `Haber Yeri`/`Etiketler` gibi meta etiketleri, kaynakça bölümleri (`Ayrıca bakınız`, `Notlar`, `Kaynakça`, `See also`, `References`) ve başlığı kaçırılmış tekil kaynakça satırları (`↑ ...`, `Erişim tarihi: ...`, `ISBN ...`, `ss. 12-14`, `... tarihinde kaynağından arşivlendi`) atılır. Çıplak bağlantı/sosyal medya satırları (`https://t.co/...`), telif satırları (`© ... Tüm hakları saklıdır`) ve menü/eylem etiketleri (`Ana Sayfa`, `Devamını oku`, `Çerez Politikası`, `Abone ol`) da gövdeye girmez. Bu temizlik **tarama sırasında, kayıt yazılmadan önce** çıkarma aşamasında uygulanır (`export.rs::sanitize_markdown`); hem `txt` hem `jsonl` çıktısı aynı temiz metni saklar, filtreler de temizlenmiş metni ölçer. Aynı fonksiyon `export-txt` ile mevcut txt shard'larına da uygulanır; `18. yüzyıl` gibi sıra sayıları bozulmaz.
* **Başlık kapısı**: başlık yalnızca metinle ilgiliyse eklenir. Karar, başlığın içerik kelimelerinin (stopword'ler çıkarılmış, ≥3 karakter) gövdedeki kelimelerle örtüşmesine bakar: en az bir eşleşme **ve** oran ≥ 0.34. "Ana Sayfa", "Gündem | Spor | Ekonomi" düşer; "Türkiye - Vikipedi" kalır. `output.include_title = false` ile kapatılır.
* **Ayırıcı**: `output.separator` (varsayılan `<veribitisi>`) her belgeden sonra kendi satırında yazılır.

Denetim/taşınabilirlik için JSONL formatı korunur: `output.format = "jsonl"` ile `shard_00000.jsonl.zst` üretilir (Zstd seviye `output.zstd_level`). Her satır:

```json
{
  "id": "blake3_256_hex",
  "url": "https://example.com/blog/rust",
  "domain": "example.com",
  "title": "Rust ile Sistem Programlama",
  "language": "tr",
  "language_score": 0.9921,
  "word_count": 845,
  "char_count": 5620,
  "simhash": "0x9f8e7d6c5b4a3928",
  "scraped_at": "2026-11-25T22:30:00Z",
  "text": "# Rust ile Sistem Programlama\n\n..."
}
```

`manifest.json` (atomik: `manifest.json.tmp` → `rename`):

```json
{
  "version": 2,
  "format": "txt",
  "separator": "<veribitisi>",
  "created_at": "2026-11-25T22:00:00Z",
  "updated_at": "2026-11-25T22:31:00Z",
  "shard_size_bytes": 268435456,
  "documents": 12034,
  "words": 8123456,
  "bytes_uncompressed": 55900000,
  "shards": [
    {
      "name": "corpus_00000.txt",
      "size_bytes": 26850000,
      "uncompressed_bytes": 26850000,
      "documents": 512,
      "words": 401233,
      "sha256": "…64 hex…"
    }
  ],
  "rejects": { "too_few_words": 412, "near_duplicate": 98, "unsupported_language": 31 }
}
```

### 4.1 Shard'ları tek dosyada birleştirme (`export-txt`)

Tarama zaten txt yazar; `export-txt` bütün shard'ları tek bir `corpus.txt` içinde birleştirir ve istenirse ayırıcıyı yeniden yazar:

```bash
pretrain-scraper export-txt --dataset ./dataset --output ./dataset/corpus.txt
pretrain-scraper export-txt --dataset ./dataset --separator "===DOC==="
```

* `txt` shard'ları okunur, bloklara ayrılır ve seçilen ayırıcıyla yeniden yazılır.
* `jsonl`/`jsonl.zst` dataset'lerde Markdown temizliği, başlık kapısı ve `--no-title` burada uygulanır; böylece eski dataset'ler de düz metne çevrilebilir.
* Varsayılan olarak korpus geneli tekilleştirme uygulanır: aynı paragraf/cümle/blok ikinci kez yazılmaz. Eski veya elle birleştirilmiş dataset'lerdeki yinelemeleri de temizler; `--no-dedupe` ile kapatılır.
* Komut; okunan shard sayısını, belge/kelime toplamını, dosya boyutunu, atılan yineleme sayısını ve (jsonl modunda) kaç başlığın tutulup kaçının atıldığını raporlar.

---

## 5. Konfigürasyon

Tüm anahtarlar ve varsayılanları `config.example.toml` içinde açıklanmıştır. Öne çıkanlar:

| Anahtar | Varsayılan | Etki |
|---|---|---|
| `general.output_dir` / `db_path` | `dataset` / `crawl_db` | Shard'lar ve gömülü DB |
| `frontier.max_depth` | `3` | Link derinliği |
| `frontier.min_delay_ms` | `1000` | Domain başına minimum istek aralığı |
| `frontier.max_concurrency_per_domain` | `2` | Domain başına eşzamanlı istek |
| `frontier.respect_crawl_delay` | `true` | robots.txt `Crawl-delay` üst sınırı |
| `frontier.discover_sitemaps` | `true` | robots.txt sitemap keşfi (büyük site listelerinde kapatın) |
| `frontier.allowed_domains` / `blocked_domains` | `[]` | eTLD+1 bazlı kapsam |
| `frontier.allowed_domains_file` | `""` | Site listesi txt dosyası; kapsam + varsayılan seed'ler |
| `frontier.max_pages` | `0` | Sayfa bütçesi (0 = sınırsız) |
| `fetch.max_response_bytes` | `5 MiB` | Streaming gövde tavanı |
| `fetch.max_retries` | `3` | 429/5xx/transport için retry |
| `extract.min_block_chars` | `80` | Aday blok eşiği |
| `extract.max_link_ratio` | `0.45` | Link çiftliği eşiği |
| `filter.min_words` / `max_words` | `50` / `1000000` | Uzunluk filtresi (kısa bültenler ve uzun belgeler kabul edilir) |
| `filter.target_languages` | `["en","tr"]` | ISO-639-1 (veya 639-3) |
| `filter.min_lang_confidence` | `0.85` | Dil güven eşiği |
| `filter.max_duplicate_line_ratio` | `0.25` | Tekrar filtresi |
| `filter.max_ngram_repetition_ratio` | `0.30` | En sık n-gram payı / tekrar eden n-gram payı |
| `dedup.max_hamming_distance` | `3` | SimHash yakın kopya eşiği |
| `output.format` | `txt` | `txt` = düz corpus, `jsonl` = kayıtlar |
| `output.separator` | `<veribitisi>` | txt ayırıcısı (tek satır) |
| `output.include_title` | `true` | Başlık yalnızca metinle ilgiliyse |
| `output.max_chunk_words` | `3000` | Konu kaydı üst sınırı (kelime) |
| `output.min_chunk_words` | `25` | Daha kısa bölümler sonrakiyle birleşir |
| `output.deduplicate` | `true` | Paragraf/cümle/blok düzeyinde korpus geneli tekilleştirme |
| `output.dedup_min_sentence_words` | `6` | Tekilleştirmeye giren en kısa paragraf/cümle |
| `output.dedup_index_capacity` | `500000` | Tekilleştirme indeksi üst sınırı (sabit bellek) |
| `output.saved_urls_file` | `crawled_urls.txt` | Kaydedilen içerik sayfası URL'leri; sonraki taramalarda atlanır (`""` kapatır) |
| `output.shard_size_bytes` | `268435456` | Rotasyon boyutu |
| `output.zstd_level` | `5` | 1-22 (yalnız `jsonl`) |
| `pipeline.channel_capacity` | `256` | Aşamalar arası backpressure |
| `pipeline.fetch_workers` | `32` | Eşzamanlı HTTP görevi |
| `pipeline.extract_workers` / `filter_workers` | `0` (=CPU) | CPU havuzu |
| `pipeline.dedup_workers` | `0` (=CPU/2) | Dedup havuzu |

Doğrulama katıdır: bilinmeyen anahtar, aralık dışı eşik veya boş dil listesi programı hemen durdurur (`ScraperError::Config`).

---

## 6. Mimari notlar

### 6.1 Frontier ve politeness
* Kuyruk anahtarı `[ters_öncelik][sıra_no]` olduğu için `first()` her zaman en yüksek öncelikli ve en eski URL'i verir; sıralama bellekte yapılmaz.
* `visited` ağacındaki atomik `compare_and_swap` "claim" olarak kullanılır: aynı linki iki worker keşfetse bile yalnızca biri kuyruğa girer.
* Domain başına ayrı `Semaphore` (eşzamanlılık) ve async `Mutex<Instant>` (gecikme) vardır; bir domain diğerini kilitlemez.
* robots.txt bellek + LSM cache'inde TTL ile tutulur; sitemap URL'leri keşfedilip `<loc>` girdileri frontier'a eklenir.

### 6.2 Fetch
* `reqwest` + rustls, gzip/brotli/zstd/deflate, cookie jar, redirect limiti.
* Gövde `bytes_stream()` ile parça parça okunur; limit aşımında transfer iptal edilir.
* Yalnızca `text/html` ve `application/xhtml+xml` kabul edilir; charset header → `<meta charset>` → lossy UTF-8 sırasıyla çözülür.
* 403/404/410 kalıcı hata sayılır (retry yok); 429/5xx/transport `Retry-After` veya exponential backoff + jitter ile denenir.

### 6.3 Extraction
* `scraper` (html5ever) ile parse edilir, ardından **tek geçişli** ağaç yürüyüşü yapılır.
* Budama iki sinyalle: etiket adı ve `class`/`id`/`role`/`aria-label` desenleri; ayrıca `hidden`, `aria-hidden=true`, `display:none` atılır.
* Skorlama: blok başına metin uzunluğu, `<p>` sayısı, link metni oranı ve taşıdığı etiket sayısı. Link oranı > 0.45 olan bloklar elenir; en büyük metin kütlesinin %65'ini taşıyan **en sıkı** kapsayıcı seçilir (böylece `<div class="wrapper">` değil `<article>` kazanır).
* Çıktı NFKC normalize edilir, kontrol karakterleri temizlenir, 3+ boş satır `\n\n`'ye indirgenir.

### 6.4 Filtre sırası
`uzunluk → kelime şekli → noktalama/alfabe/sembol → engelli ifade → tekrar (satır + n-gram) → dil → PII maskeleme`.

Tekrar aşaması üç sinyali birlikte değerlendirir: yinelenen satır oranı, yinelenen karakter oranı, ve n-gram tekrar oranları (en sık n-gram payı ve `1 - benzersiz/toplam`).

Her ret tek bir `RejectReason` üretir ve atomik sayaçlara yazılır; manifestte kırılım olarak görünür.

### 6.5 Dedup
* Exact: NFKC + lowercase + yalnızca alfanümerik kelimeler → XXH3-128 → kalıcı ağaç (atomic claim).
* Yakın kopya: 5-kelimelik shingle'lar → 64-bit SimHash → 4 × 16-bit bant. Sorgu her bantta bucket taraması yapar, Hamming ≤ 3 ise reddeder.
* Şablon paragraf temizliği: eşik üstü tekrar eden paragraflar (başlıklar hariç) belgeden çıkarılır; sayaç haritası `paragraph_dedup_max_entries` ile sınırlıdır.
* Korpus geneli tekilleştirme (yazma/export): kanonik paragraf ve cümle hash'leri tek bir sabit kapasiteli kümede tutulur; ikinci kez görülen metin atılır. Tüm paragrafları yinelenen blok hiç yazılmaz. Bu aşama `documents` sayacına yalnızca gerçekten yazılan kayıtları ekler ve `duplicates cut` olarak raporlanır.
* `verify`: uzun satırların (≥10 kelime) belgeler arası tekrarını ve blok düzeyinde SimHash yakın kopyalarını (Hamming ≤ 3) sayar. Yakın kopya varsa veya uzun satır tekrarı belge sayısının %1'ini aşarsa doğrulama başarısız olur; böylece "temiz" iddiası ölçülebilir kalır.

### 6.6 Dayanıklılık
* Her URL için bir `CompletionGuard` taşınır; yazıldığında, reddedildiğinde, atlandığında veya hata aldığında tam olarak bir kez "tamamlandı" sayılır. Frontier besleyici, kuyruk boş **ve** `dispatched == completed` olduğunda durur.
* `SIGINT`/`SIGTERM`: besleyici durur, kanallar kapanır, stage'ler eldeki işi bitirir, shard `finish()` + `sync_all()`, manifest `tmp → rename`, DB `flush()`.
* Manifest her shard rotasyonunda ve kapanışta yeniden yazılır; `verify` SHA-256, Zstd çerçevesi, JSON şeması, belge/kelime sayıları ve manifest toplamlarını karşılaştırır.

### 6.7 Eşzamanlılık
* Global kilit yok. CPU aşamaları `spawn_blocking` + semaphore ile çalışır; kanallar bounded olduğu için yavaş downstream doğal backpressure uygular.
* Telemetri tamamen `AtomicU64`; dashboard ayrı bir görevde sadece okuma yapar.

---

## 7. Performans ayarları

| Amaç | Öneri |
|---|---|
| Tek makinede maksimum throughput | `pipeline.fetch_workers = 128`, `channel_capacity = 1024`, `extract_workers/filter_workers = CPU`, `dedup_workers = CPU/2` |
| Nazik tarama | `min_delay_ms = 2000`, `max_concurrency_per_domain = 1`, `respect_crawl_delay = true` |
| Yüksek kalite, küçük hacim | `min_words = 300`, `min_lang_confidence = 0.95`, `max_hamming_distance = 2` |
| Depolama tasarrufu | `zstd_level = 9`, `shard_size_bytes = 536870912` |
| Sadece hızlı prototip | `filter.min_words = 30`, `target_languages = ["en"]`, `zstd_level = 3` |

Pratik gözlemler:

* Darboğaz genelde ağdır; `fetch_workers` CPU sayısından çok daha yüksek olabilir.
* `dedup_workers`'ı CPU/2 üzerine çıkarmak sled yazma rekabetini artırır, kazanç sağlamaz.
* Zstd seviye 3-6 arası metinde iyi sıkıştırma/hız dengesi verir; >9 tek çekirdekte darboğaz olur.
* RAM tüketimi `channel_capacity × (belge boyutu + payload)` ile sınırlıdır; belge boyutu `fetch.max_response_bytes` ile tavanlanır.

---

## 8. Testler

```bash
cargo test
```

Kapsam:

* `frontier::normalizer`: tracking parametre temizliği, sıralama, relative çözümleme, binary/şema reddi, eTLD+1, domain filtreleri.
* `frontier::robots`: grup seçimi, `Allow`/`Disallow` önceliği, `*`/`$` joker, `Crawl-delay`, sitemap.
* `frontier::scheduler`: domain başına serileştirme, crawl-delay, domainler arası bağımsızlık.
* `frontier::store`: öncelik/FIFO, duplicate claim, bütçe, yeniden açılışta kalıcılık.
* `extractor`: gürültülü HTML'den nav/cookie/sidebar/yorum/reklam temizliği, Markdown yapısı, kod fence, tablo, link keşfi.
* `filter`: kısa metin, menü yığını, spam tekrarı, sembol bombardımanı, engelli ifade, dil.
* `pii`: e-posta, Luhn kart, IPv4, telefon, yanlış pozitif kontrolü.
* `dedup`: exact duplicate, paragraf şablonu, SimHash %95 benzer metin yakalama, farklı metin geçirme, LSH kalıcılığı.
* `storage`: shard rotasyonu, manifest SHA-256, resume numaralandırma, boş shard temizliği, şema doğrulama, yineleme tespiti.
* `export`: Markdown→düz metin dönüşümü, `<veribitisi>` ayırıcı, başlık ilgililik kapısı (belge başına bir kez), korpus geneli paragraf/cümle/blok tekilleştirme, kaynakça ve meta gürültüsü temizliği, özel ayırıcı ve hata yolları.
* `tests/pipeline_e2e.rs`: yerel mock HTTP sunucusu ile çok sayfalı crawl, 1 KiB shard limitiyle çoklu shard, `verify` ile SHA-256/şema kontrolü, bozulmuş shard tespiti, `clean-html` modu ve tekrar çalıştırmada tam dedup.

---

## 9. Bilinen sınırlar

* Sitemap **index** dosyaları iç içe açılmaz; yalnızca ilk seviye `<loc>` girdileri değerlendirilir.
* JPEG/PDF gibi ikili içerikler hiç kuyruğa alınmaz (bilinçli tercih); OCR/PDF çıkarımı kapsam dışıdır.
* WARC konteynerleri doğrudan okunmaz; `jsonl`/`jsonl.zst` veya HTML dosyaları beklenir.
* Dil tespiti `whatlang` iledir; çok kısa belgelerde güven skoru düşük olduğu için reddedilir (`LowLanguageConfidence`).
* Şifreleme/anahtar yönetimi yoktur: gömülü DB ve shard'lar diskte açıktır; hassas ortamda disk şifrelemesi kullanın.

---

## 10. Lisans

MIT.
