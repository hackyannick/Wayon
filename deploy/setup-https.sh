#!/usr/bin/env bash
# Richtet NGINX mit HTTPS (Let's Encrypt) vor Wayon ein.
#
# Voraussetzungen:
#   - Debian/Ubuntu, als root ausführen
#   - Die Domain zeigt per DNS (A/AAAA-Eintrag) auf diesen Server
#   - Ports 80 und 443 sind in der Firewall offen
#   - Wayon läuft als Dienst auf 127.0.0.1:8080 (deploy/wayon.service)
#
# Aufruf:
#   sudo deploy/setup-https.sh suche.meinedomain.de admin@meinedomain.de

set -euo pipefail

DOMAIN="${1:-}"
EMAIL="${2:-}"
if [[ -z "$DOMAIN" || -z "$EMAIL" ]]; then
    echo "Aufruf: $0 <domain> <e-mail für Let's Encrypt>" >&2
    exit 1
fi
if [[ ! "$DOMAIN" =~ ^[A-Za-z0-9.-]+$ ]]; then
    echo "Ungültige Domain: $DOMAIN" >&2
    exit 1
fi
if [[ $EUID -ne 0 ]]; then
    echo "Bitte als root ausführen (sudo)." >&2
    exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"

# Ohne IPv6 im Kernel würde nginx an "listen [::]" scheitern
strip_ipv6() {
    if [[ ! -e /proc/net/if_inet6 ]]; then
        sed -i '/listen \[::\]/d' "$1"
    fi
}
TEMPLATE="$HERE/nginx/wayon.conf"
CONF=/etc/nginx/sites-available/wayon.conf

echo "==> Pakete installieren (nginx, certbot)"
apt-get update -q
apt-get install -y -q nginx certbot

mkdir -p /var/www/certbot

if [[ ! -f "/etc/letsencrypt/live/$DOMAIN/fullchain.pem" ]]; then
    echo "==> Vorläufige HTTP-Konfiguration für die Zertifikatsprüfung"
    cat > "$CONF" <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name $DOMAIN;
    location /.well-known/acme-challenge/ { root /var/www/certbot; }
    location / { return 503; }
}
EOF
    strip_ipv6 "$CONF"
    ln -sf "$CONF" /etc/nginx/sites-enabled/wayon.conf
    nginx -t
    systemctl reload nginx

    echo "==> Zertifikat von Let's Encrypt anfordern"
    certbot certonly --webroot -w /var/www/certbot -d "$DOMAIN" \
        --email "$EMAIL" --agree-tos --non-interactive
fi

echo "==> Endgültige HTTPS-Konfiguration installieren"
sed "s/suche\.example\.de/$DOMAIN/g" "$TEMPLATE" > "$CONF"
strip_ipv6 "$CONF"
ln -sf "$CONF" /etc/nginx/sites-enabled/wayon.conf
nginx -t
systemctl reload nginx

echo "==> Automatische Verlängerung: nginx nach jeder Erneuerung neu laden"
mkdir -p /etc/letsencrypt/renewal-hooks/deploy
cat > /etc/letsencrypt/renewal-hooks/deploy/reload-nginx.sh <<'EOF'
#!/bin/sh
systemctl reload nginx
EOF
chmod +x /etc/letsencrypt/renewal-hooks/deploy/reload-nginx.sh
certbot renew --dry-run

echo
echo "Fertig: https://$DOMAIN"
