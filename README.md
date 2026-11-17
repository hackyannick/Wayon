# Wayon

Wayon ist eine Websuchmaschine in **Rust** mit eigenem Crawler, eigenem invertierten Index und einer Weboberfläche wie bei großen Suchmaschinen. Sie ist auf geringen Ressourcenverbrauch ausgelegt: ein einziges Binary (ca. 10 MB), keine Datenbank, keine JVM und kein Garbage Collector. Der Index wird per `mmap` gelesen.

![Startseite mit Autovervollständigung](docs/startseite.png)
![Ergebnisseite](docs/ergebnisse.png)

## Installation

Wayon läuft unter Linux, macOS und Windows. Für einen Dauerbetrieb ist ein Linux-Server (oder WSL2 unter Windows) am einfachsten.

### 1. Voraussetzungen

**Rust** (Version 1.88 oder neuer) über rustup installieren:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Dazu kommen ein **C-Compiler** und **git**, weil die TLS-Bibliothek C-Code enthält:

| System | Befehl |
|---|---|
| Debian / Ubuntu | `sudo apt install build-essential git` |
| Fedora | `sudo dnf install gcc gcc-c++ make git` |
| Arch | `sudo pacman -S base-devel git` |
| macOS | `xcode-select --install` |
| Windows | [rustup-init.exe](https://rustup.rs) und dazu „Visual Studio Build Tools“ mit „Desktopentwicklung mit C++“. Falls der Build bei `aws-lc-sys` scheitert, zusätzlich [CMake](https://cmake.org) und [NASM](https://nasm.us) installieren. |

Falls der Build bei `aws-lc-sys` mit einer CMake-Meldung abbricht, `cmake` nachinstallieren (z. B. `sudo apt install cmake`).

### 2. Herunterladen und bauen

```bash
git clone https://github.com/hackyannick/Wayon.git
cd Wayon
cargo install --path .          # baut und legt `wayon` nach ~/.cargo/bin
wayon --version
```

Der erste Build dauert einige Minuten. Alternativ baut `cargo build --release` nur das Programm unter `target/release/wayon`; das lässt sich überall hin kopieren, z. B. nach `/usr/local/bin/`.

### 3. Crawlen, indexieren, suchen

```bash
# Crawl mit den mitgelieferten 100 Start-Seiten (siehe unten)
wayon crawl --seeds-file seeds/top100.txt --stay-on-domains \
            --max-per-host 2000 --max-pages 200000 --concurrency 100

wayon index                          # Index bauen
wayon serve --addr 127.0.0.1:8080    # http://127.0.0.1:8080 im Browser öffnen
wayon search "klimawandel folgen"    # oder im Terminal suchen
```

Alle Daten liegen unter `--data` (Standard `./data`): `data/crawl/` enthält die Rohdaten, `data/index/` den Suchindex. Ein Crawl lässt sich jederzeit mit Strg+C abbrechen; der nächste `wayon crawl` setzt dort fort, wo er aufgehört hat (Start-URLs müssen dann nicht erneut angegeben werden).

### Die Start-Liste `seeds/top100.txt`

Sie enthält 100 inhaltsreiche, gut crawlbare Websites, überwiegend deutschsprachig: Nachschlagewerke, Nachrichten (DE/AT/CH/international), Technik, Programmier-Dokumentation, Wissenschaft, Behörden und Recht, Gesundheit sowie Alltag und Kultur. Soziale Netzwerke, Shops und Videoportale fehlen bewusst: Sie sperren Crawler oder liefern ohne Browser kaum Text. Die Liste ist eine normale Textdatei, also einfach erweitern oder kürzen.

- `--stay-on-domains` hält den Crawl auf diesen Websites. Ohne die Option folgt Wayon auch Links zu fremden Domains und wächst unkontrolliert ins ganze Web.
- `--max-per-host 2000` × 100 Websites ergibt ca. 200.000 Seiten. Wayon fragt jede Website höchstens einmal pro Sekunde an. Damit dauert das je nach Leitung grob 1–3 Stunden und braucht ungefähr 1–2 GB Platz für Rohdaten und Index zusammen (grobe Schätzung, je nach Seiten).
- Websites, deren robots.txt fremde Crawler verbietet, werden automatisch übersprungen.

### Als Dienst betreiben (Linux, systemd)

Im Repository-Ordner, nach `cargo install --path .`:

```bash
sudo deploy/install-service.sh
```

Das Skript legt den Systembenutzer `wayon` an, installiert das Programm nach `/usr/local/bin`, richtet `/var/lib/wayon` ein (inklusive Seed-Liste als `seeds.txt`), übernimmt einen bereits in `./data` gecrawlten Bestand und startet den Dienst. Es lässt sich nach jedem Update erneut ausführen.

Crawl und Index laufen danach als Benutzer `wayon` und im selben Datenverzeichnis wie der Dienst:

```bash
sudo -u wayon wayon --data /var/lib/wayon crawl --seeds-file /var/lib/wayon/seeds.txt --stay-on-domains
sudo -u wayon wayon --data /var/lib/wayon index
```

Ein Neustart ist nicht nötig: Der Dienst prüft alle 5 Sekunden, ob ein neuer Index vorliegt, und übernimmt ihn im laufenden Betrieb. Solange noch kein Index existiert, zeigt die Startseite einen Hinweis (HTTP 503).

Der Dienst lauscht nur auf `127.0.0.1:8080` und ist damit von außen nicht erreichbar. Öffentlich zugänglich wird er über NGINX mit HTTPS (nächster Abschnitt). Regelmäßiges Nachcrawlen geht z. B. per Cronjob mit denselben zwei Befehlen.

### HTTPS mit NGINX und Let's Encrypt

Voraussetzungen:
- Ein Debian- oder Ubuntu-Server, auf dem Wayon wie oben als Dienst läuft
- Eine Domain, deren DNS-Eintrag (A, bei IPv6 auch AAAA) auf den Server zeigt
- Offene Ports 80 und 443, z. B. mit `sudo ufw allow 80,443/tcp`

Dann genügt ein Befehl:

```bash
sudo deploy/setup-https.sh suche.meinedomain.de admin@meinedomain.de
```

Das Skript
1. installiert `nginx` und `certbot`,
2. holt ein kostenloses Zertifikat von Let's Encrypt,
3. installiert die Konfiguration [`deploy/nginx/wayon.conf`](deploy/nginx/wayon.conf) mit deiner Domain und
4. richtet die automatische Verlängerung ein (certbot-Timer; NGINX wird nach jeder Erneuerung neu geladen).

Danach ist die Suche unter `https://suche.meinedomain.de` erreichbar. Die Konfiguration bietet:
- Weiterleitung von HTTP auf HTTPS, HTTP/2, nur TLS 1.2 und 1.3
- Sicherheits-Header: HSTS, Content-Security-Policy, Schutz vor Einbettung in fremde Seiten
- **Rate-Limit pro IP:** Suche 5 Anfragen/s (Burst 20), Autovervollständigung 20/s (Burst 40); darüber antwortet NGINX mit 429. Die Werte stehen oben in `wayon.conf` und lassen sich dort anpassen.
- gzip-Kompression, nur GET/HEAD erlaubt, Keep-Alive zum Backend

Wer die Konfiguration lieber von Hand einrichtet: `deploy/nginx/wayon.conf` nach `/etc/nginx/sites-available/` kopieren, `suche.example.de` durch die eigene Domain ersetzen, Zertifikat mit `certbot certonly --webroot -w /var/www/certbot -d <domain>` holen, verlinken und `nginx -t && systemctl reload nginx`.

### Befehle

| Befehl | Zweck |
|---|---|
| `crawl [SEEDS…]` | Seiten laden. Wichtige Optionen: `--seeds-file`, `--max-pages`, `--max-depth`, `--max-per-host`, `--concurrency`, `--delay-ms`, `--stay-on-domains`, `--domains a.de,b.de` |
| `index [--mem-mb 512]` | Index aus allen gecrawlten Seiten neu aufbauen; der fertige Index wird atomar ausgetauscht |
| `serve [--addr]` | Weboberfläche, JSON-API und Autovervollständigung starten |
| `search QUERY` | Suche im Terminal |
| `stats` | Dokumente, Terme und Größe jeder Indexdatei anzeigen |
| `dump [--filter]` | Gecrawlte Rohdaten prüfen |

### Suchsyntax

| Eingabe | Bedeutung |
|---|---|
| `rust crawler` | Alle Begriffe müssen vorkommen; Füllwörter wie „die“ oder „the“ sind optional |
| `"exakte phrase"` | Wörter direkt hintereinander, auch mit „…“ |
| `-wort` | Seiten mit diesem Wort ausschließen |
| `site:example.com` | Nur diese Domain inklusive Subdomains |
| `-site:example.com` | Diese Domain ausschließen |

### HTTP-API

- `GET /api/search?q=…&page=1&n=10` liefert Treffer als JSON (Titel, URL, Snippet, Score, Trefferzahl, Korrektur).
- `GET /api/suggest?q=…` liefert Vorschläge im OpenSearch-Format.
- `GET /opensearch.xml` macht Wayon im Browser als Suchmaschine hinzufügbar.

### Fehlersuche

**NGINX meldet „502 Bad Gateway“:** NGINX erreicht Wayon auf `127.0.0.1:8080` nicht, der Dienst läuft also nicht. Prüfen:

```bash
sudo systemctl status wayon                    # läuft der Dienst?
sudo journalctl -u wayon -n 30 --no-pager      # Fehlermeldung von Wayon
curl -I http://127.0.0.1:8080/                 # antwortet Wayon direkt?
sudo tail -n 20 /var/log/nginx/error.log       # was NGINX sieht
```

Häufige Ursachen:

| Meldung im Journal | Lösung |
|---|---|
| `status=203/EXEC` oder „No such file“ | Programm fehlt unter `/usr/local/bin/wayon`: `sudo cp ~/.cargo/bin/wayon /usr/local/bin/` |
| `Adresse 127.0.0.1:8080 nicht verfügbar` | Port belegt: `sudo ss -ltnp 'sport = :8080'` zeigt, welches Programm ihn nutzt |
| `Permission denied` bei `/var/lib/wayon` | Daten gehören einem anderen Benutzer: `sudo chown -R wayon:wayon /var/lib/wayon` |
| `status=217/USER` | Benutzer `wayon` fehlt: `sudo deploy/install-service.sh` ausführen |
| `Kein Index in …` (ältere Wayon-Versionen) | Index fehlt: `wayon index` ausführen oder Wayon aktualisieren; neue Versionen starten auch ohne Index |

**Startseite zeigt „Der Suchindex wird gerade aufgebaut oder fehlt noch“:** Wayon läuft, findet aber unter `/var/lib/wayon/index` keinen Index. Crawl und Index müssen mit `--data /var/lib/wayon` laufen, nicht im Repository-Ordner (dort landen sie sonst in `./data`).

## Architektur

```
 Crawler ──► Crawl-Speicher ──► Index-Builder ──► Index (mmap) ──► Suche ──► HTTP/Web-UI
 (tokio)     (zstd-Segmente)    (rayon, SPIMI)    (FST + Bitpacking)
```

### Crawler (`src/crawler/`)
- Asynchron mit tokio und reqwest (rustls, HTTP/2, gzip/brotli). Pro Host läuft höchstens eine Anfrage gleichzeitig, mit Mindestpause dazwischen.
- **robots.txt** nach RFC 9309: User-agent-Gruppen, `Allow`/`Disallow` mit `*` und `$`, längste Regel gewinnt, `Crawl-delay`. **Sitemaps** werden gelesen, auch Sitemap-Indizes.
- `noindex`/`nofollow` aus Meta-Tag und `X-Robots-Tag` werden beachtet, ebenso `rel=nofollow`, `rel=canonical` und `<base href>`.
- Bei 429/503 wird die Pause verdoppelt bzw. `Retry-After` befolgt; nach erfolgreichen Anfragen sinkt sie wieder.
- Bereits gesehene URLs merkt sich ein **Bloom-Filter**: rund 1,2 Byte pro URL statt rund 100 Byte in einem HashSet.
- URL-Normalisierung: Fragmente, Tracking-Parameter (`utm_*`, `gclid` …) und Standardports werden entfernt. Die Zeichenkodierung wird erkannt (Header, BOM, `<meta charset>`).
- Bei der Textextraktion werden Navigation, Footer, Sidebars, Cookie-Banner, Scripts usw. übersprungen.
- Bei Abbruch wird die offene Warteschlange gespeichert; der nächste `crawl`-Aufruf setzt dort fort.

### Crawl-Speicher (`src/store.rs`)
Ein append-only Speicher: Jede Seite ist ein eigener zstd-Frame in Segmentdateien zu je 256 MB. Defekte oder abgeschnittene Records, etwa nach einem Absturz, werden übersprungen. Records lassen sich direkt adressieren und per mmap lesen.

### Index (`src/index/`)
Der Aufbau läuft in mehreren Phasen:
1. **Auswahl:** Pro URL zählt die neueste Version. `noindex`- und Fehlerseiten fallen weg. `rel=canonical` und inhaltsgleiche Duplikate werden zusammengefasst (Hash des Textes).
2. **Doc-IDs nach umgekehrtem Host sortiert** (`com.example.www/pfad`). Seiten derselben Domain liegen dadurch nebeneinander. Das ergibt kleine Doc-ID-Abstände für die Kompression und schnelle `site:`-Filter über ID-Bereiche.
3. **Linkgraph:** Ankertexte eingehender Links werden zum eigenen Suchfeld. Dazu kommt ein gewichteter **PageRank**, bei dem Links innerhalb derselben Domain weniger zählen.
4. **Invertierung (SPIMI, parallel):** Die Dokumente werden auf allen Kernen analysiert. Die Postings werden nach Term-Hash auf Shards verteilt, und jeder Shard läuft auf einem eigenen Thread. Ist das RAM-Budget (`--mem-mb`) erreicht, wird ein sortierter Lauf auf die Platte geschrieben.
5. **Merge:** Ein k-Wege-Merge erzeugt aus allen Läufen das kompakte Endformat.

**Dateiformat, auf geringen Platzbedarf optimiert:**

| Datei | Inhalt |
|---|---|
| `terms.fst` | Wörterbuch als *Finite State Transducer*. Gemeinsame Präfixe **und** Suffixe aller Terme werden geteilt; das ist meist kleiner als die Terme als reiner Text. |
| `postings.bin` | Pro Term Blöcke à 128 Dokumente. Doc-ID-Deltas und Termfrequenzen der 5 Felder (Titel, Überschriften, Text, URL, Ankertext) sind **bitgepackt**, Felder ohne Treffer kosten 1 Byte pro Block. Eine Skip-Tabelle erlaubt schnelles Springen, die Positionen (für Phrasen und Nähe) sind als Varint-Deltas gespeichert. |
| `docmeta.bin` | **8 Byte pro Dokument**: Feldlängen logarithmisch auf je 1 Byte quantisiert, PageRank, Sprache, URL-Tiefe |
| `docstore.bin/.idx` | Je 16 Dokumente (URL, Titel, Beschreibung, Text für Snippets) bilden einen zstd-Block (Level 12) |
| `hosts.bin` | Host → Doc-ID-Bereich |
| `words.fst` | Oberflächenwörter mit Häufigkeit, für „Meinten Sie“ und die Autovervollständigung |

### Suche und Ranking (`src/search/`)
- Die Auswertung läuft **Dokument für Dokument** über alle Postinglisten. Der seltenste Begriff führt, die anderen springen per Skip-Tabelle hinterher.
- **Stemming** für Deutsch und Englisch; Akzente und Umlaute werden normalisiert (ä→a, ß→ss). Ein Suchwort sucht automatisch nach allen Sprachvarianten, „Suchmaschinen“ findet also auch „Suchmaschine“.
- Das **Ranking** ist zweistufig:
  1. Alle Treffer werden mit **BM25F** über 5 gewichtete Felder bewertet, dazu PageRank, Bonus wenn alle Begriffe im Titel stehen, Bonus wenn ein Suchwort im Domainnamen steht (z. B. „github“ → github.com, Startseite bevorzugt) und ein Abzug für URL-Tiefe.
  2. Die besten 1.000 werden nach **Termnähe** neu sortiert (kleinstes Fenster, das alle Begriffe enthält).
- **Domain-Diversität:** Höchstens 2 Treffer pro Host stehen am Stück, weitere rücken nach hinten.
- **Gelockerte Suche:** Gibt es fast keine Treffer, darf ein Begriff fehlen.
- **Rechtschreibkorrektur:** Ein Levenshtein-Automat läuft über `words.fst` und erkennt auch Buchstabendreher. Ohne Treffer wird automatisch korrigiert („Ergebnisse für … werden angezeigt“).
- **Snippets** zeigen das Textfenster mit den meisten verschiedenen Suchbegriffen, die Treffer sind hervorgehoben.

## Messwerte

Gemessen auf 4 CPU-Kernen.

**Echter Crawl** (pkg.go.dev und pypi.org, 289 Dokumente nach Deduplizierung):

| | Wert |
|---|---|
| RAM Crawler | ca. 50–65 MB (davon 11,7 MB Bloom-Filter für 10 Mio. URLs) |
| RAM Server | **ca. 10 MB** nach 200 Anfragen (beim 200.000-Seiten-Index ca. 29 MB inklusive gemappter Indexseiten) |
| Antwortzeit | 1–4 ms |

**Synthetischer Lasttest**, erzeugbar mit `cargo run --release --example synth -- /tmp/synth 200000`:

| 200.000 Seiten, im Schnitt 450 Wörter | Wert |
|---|---|
| Rohtext im Crawl-Speicher | 305 MB |
| Index gesamt | 466 MB, davon invertierter Index **1,3 KB pro Dokument** inklusive Positionen; der Rest ist der komprimierte Dokumentspeicher für Snippets |
| Indexaufbau | ca. 2 min, Heap ca. 200–340 MB je nach `--mem-mb` |
| Suche: seltene Begriffe | 1–3 ms |
| Suche: sehr häufige Begriffe (Treffer in 100 % der Seiten) | 10–30 ms |
| Phrase aus zwei sehr häufigen Wörtern | ca. 80 ms |

## Tests

```bash
cargo test
```

Es gibt Unit-Tests für Kodierung, Postings, robots.txt, HTML-Extraktion, Tokenizer, Ranking-Hilfen usw. Dazu kommt ein **End-to-End-Test** (`tests/e2e.rs`): Er startet eine lokale Testseite, crawlt sie, baut den Index und prüft Phrasen, Ausschlüsse, `site:`, robots.txt, noindex, canonical, Duplikate, Tippfehler-Korrektur und Autovervollständigung.

## Grenzen und nächste Schritte

Wayon arbeitet mit denselben Verfahren wie große Suchmaschinen, ist aber für **eine einzelne Maschine** gebaut, also für Millionen statt Milliarden Seiten. Ausbaumöglichkeiten:
- **Block-Max-WAND**, damit Anfragen mit sehr häufigen Wörtern auf zweistelligen Millionen-Indizes schneller werden
- **Inkrementelle Index-Updates**; bisher wird der Index komplett neu gebaut, das aber parallel und mit begrenztem RAM
- **Verteilung:** den Index nach Doc-ID-Bereichen auf mehrere Maschinen aufteilen und die Ergebnisse zusammenführen
- Weitere Sprachen beim Stemming, Synonyme, Near-Duplicate-Erkennung (SimHash), Freshness-Signale, gelernte Ranking-Modelle
- JavaScript-lastige Seiten liefern ohne Headless-Browser kaum Text
