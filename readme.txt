Aegis Edge Relay - Quick README
================================

هدف
----
این پروژه یک تونل TCP-over-WebSocket می‌سازد:
User -> Bridge(Server) -> Cloudflare Worker -> Destination(Client) -> V2Ray Inbound

نکته مهم
--------
- در کانفیگ کاربر V2Ray باید آدرس/پورت Bridge را بزنید (نه سرور مقصد).
- این پروژه فقط TCP را عبور می‌دهد.

پیش‌نیاز
--------
- یک دامنه روی Cloudflare (مثال: edge.example.fun)
- Worker فعال
- یک Secret مشترک
- سرویس V2Ray روی Client (پورت لوکال inbound)

نقش‌ها
------
- Server (سمت کاربر): Bridge mode
- Client (سمت سرویس): Destination mode

نصب یک‌خطی (هر دو سرور)
-----------------------
bash -lc 'set -euo pipefail; REPO="https://github.com/YouseFMutE/Jai.git"; APP="$HOME/aegis-edge-relay"; SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update && $SUDO DEBIAN_FRONTEND=noninteractive apt-get install -y git curl ca-certificates build-essential whiptail pkg-config libssl-dev; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git curl ca-certificates gcc gcc-c++ make newt pkgconf-pkg-config openssl-devel; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git curl ca-certificates gcc gcc-c++ make newt pkgconfig openssl-devel; elif command -v pacman >/dev/null 2>&1; then $SUDO pacman -Sy --noconfirm git curl ca-certificates base-devel libnewt pkgconf openssl; else echo "Unsupported distro"; exit 1; fi; rm -rf "$APP"; git clone "$REPO" "$APP"; cd "$APP"; chmod +x deploy.sh; ./deploy.sh'

تنظیم Cloudflare Worker
----------------------
ENV ها:
- AUTH_SECRET_KEY = همان Secret مشترک
- EXIT_NODE_HOST = آی‌پی عمومی Client
- EXIT_NODE_PORT = 8443
- EXIT_NODE_SCHEME = ws

اجرای دستی (اگر نخواستید systemd)
---------------------------------
1) روی Client (Destination):
export AUTH_SECRET_KEY='YOUR_SECRET'
./target/release/aegis-edge-relay destination --listen 0.0.0.0:8443 --forward 127.0.0.1:INBOUND_V2RAY_PORT

2) روی Server (Bridge):
export AUTH_SECRET_KEY='YOUR_SECRET'
./target/release/aegis-edge-relay bridge --listen 0.0.0.0:USER_PORT --edge-addr ANYCAST_IP:443 --host YOUR_DOMAIN.fun --sni YOUR_DOMAIN.fun --path /relay --traffic-profile chrome

V2Ray Client Config
-------------------
- address = IP/Domain سرور Bridge
- port = USER_PORT
- بقیه فیلدها مثل uuid/security/path را مثل قبل نگه دارید.

بررسی سریع
----------
روی Client:
ss -lntp | grep ':8443'
ss -lntp | grep ':INBOUND_V2RAY_PORT'

روی Server:
ss -lntp | grep ':USER_PORT'
sudo journalctl -u aegis-edge-relay -n 50 --no-pager

اگر وصل نشد
-----------
- Secret در Bridge/Worker/Destination باید یکسان باشد.
- Worker باید به Client:8443 دسترسی داشته باشد.
- فایروال پورت‌های USER_PORT و 8443 را نبندد.
