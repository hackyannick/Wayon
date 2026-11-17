#!/usr/bin/env bash
# Richtet Wayon als systemd-Dienst ein (Debian/Ubuntu, als root):
#   - legt den Systembenutzer "wayon" an (falls nötig)
#   - installiert das Programm nach /usr/local/bin/wayon
#   - legt /var/lib/wayon an, kopiert die Seed-Liste dorthin
#   - übernimmt einen vorhandenen Crawl aus <repo>/data (falls dort einer liegt)
#   - installiert und startet den Dienst
#
# Aufruf aus dem Repository-Ordner, nach `cargo install --path .`:
#   sudo deploy/install-service.sh
#
# Mehrfach ausführbar (z.B. nach einem Update).

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "Bitte als root ausführen (sudo)." >&2
    exit 1
fi

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DATA=/var/lib/wayon
UNIT=/etc/systemd/system/wayon.service

# --- Programm finden: das neueste Build gewinnt ---
candidates=("$REPO/target/release/wayon" "$HOME/.cargo/bin/wayon")
if [[ -n "${SUDO_USER:-}" ]]; then
    candidates+=("$(getent passwd "$SUDO_USER" | cut -d: -f6)/.cargo/bin/wayon")
fi
BIN=""
for c in "${candidates[@]}"; do
    if [[ -x "$c" && ( -z "$BIN" || "$c" -nt "$BIN" ) ]]; then
        BIN="$c"
    fi
done
if [[ -z "$BIN" ]]; then
    echo "Kein gebautes wayon gefunden. Zuerst im Repository ausführen: cargo install --path ." >&2
    exit 1
fi

# Laufender Crawl im Repository? Dann erst beenden lassen.
if pgrep -f "wayon.* crawl" >/dev/null && [[ -d "$REPO/data/crawl" && ! -d "$DATA/crawl" ]]; then
    echo "Ein Crawl läuft noch. Bitte erst mit Strg+C beenden (wartet auf" >&2
    echo "\"offene URLs ... gespeichert\") und das Skript dann erneut starten." >&2
    exit 1
fi

echo "==> Benutzer 'wayon'"
if ! id wayon >/dev/null 2>&1; then
    useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin wayon
    echo "    angelegt"
else
    echo "    existiert bereits"
fi

echo "==> Programm: $BIN -> /usr/local/bin/wayon"
install -m 755 "$BIN" /usr/local/bin/wayon.new
mv /usr/local/bin/wayon.new /usr/local/bin/wayon

echo "==> Datenverzeichnis $DATA"
mkdir -p "$DATA"
cp "$REPO/seeds/top100.txt" "$DATA/seeds.txt"
if [[ -d "$REPO/data/crawl" && ! -d "$DATA/crawl" ]]; then
    echo "    übernehme vorhandenen Crawl aus $REPO/data/crawl"
    mv "$REPO/data/crawl" "$DATA/crawl"
    if [[ -d "$REPO/data/index" && ! -d "$DATA/index" ]]; then
        mv "$REPO/data/index" "$DATA/index"
    fi
fi
chown -R wayon:wayon "$DATA"

echo "==> systemd-Dienst"
cp "$REPO/deploy/wayon.service" "$UNIT"
systemctl daemon-reload
systemctl enable wayon >/dev/null 2>&1
systemctl restart wayon
sleep 2

if systemctl is-active --quiet wayon; then
    echo "    läuft"
else
    echo "    FEHLER – Dienst läuft nicht:" >&2
    journalctl -u wayon -n 20 --no-pager >&2 || true
    exit 1
fi

code=$(curl -s -o /dev/null -w '%{http_code}' --noproxy '*' http://127.0.0.1:8080/ || true)
echo "==> Test http://127.0.0.1:8080/ -> HTTP $code"
if [[ "$code" == "503" ]]; then
    echo
    echo "Wayon läuft, es gibt aber noch keinen Index. Jetzt bauen:"
    echo "  sudo -u wayon wayon --data $DATA index"
elif [[ "$code" != "200" ]]; then
    echo "Unerwartete Antwort – Log ansehen: journalctl -u wayon -n 50" >&2
fi

cat <<EOF

Fertig. Wichtige Befehle (immer als Benutzer wayon, mit --data $DATA):
  Crawlen:   sudo -u wayon wayon --data $DATA crawl --seeds-file $DATA/seeds.txt --stay-on-domains
  Index:     sudo -u wayon wayon --data $DATA index
  Status:    sudo -u wayon wayon --data $DATA stats
Ein neuer Index wird vom Dienst nach ein paar Sekunden automatisch übernommen.
EOF
