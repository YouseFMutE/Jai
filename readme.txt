Aegis Edge Relay - Complete Beginner Guide
==========================================

این فایل یک راهنمای کامل و ساده است برای راه‌اندازی تانل:
User -> Bridge (Server-IR) -> Cloudflare Worker -> Destination (Server-OUT) -> V2Ray Inbound

تعریف نقش‌ها (مهم)
------------------
برای جلوگیری از اشتباه، در این پروژه همیشه نقش فنی را ملاک بگیرید (نه اسم داخل/خارج):
- Bridge: سروری که کاربر مستقیم به آن وصل می‌شود.
- Destination: سروری که x-ui / V2Ray Inbound روی آن است.

اگر شما اسم‌ها را برعکس صدا می‌زنید، مشکلی نیست؛ فقط نقش‌ها باید درست ست شوند.

پیش‌نیازها
----------
1) دو سرور لینوکسی با آی‌پی عمومی
- Server-IR (Bridge)
- Server-OUT (Destination)

2) دامنه روی Cloudflare (مثال: relay.example.fun)

3) Worker فعال در Cloudflare

4) سرویس x-ui / V2Ray روی Server-OUT
- پورت inbound واقعی V2Ray را بدانید (مثال: 10000)
- این پورت، پورت پنل x-ui نیست

5) باز بودن پورت‌ها
- روی Bridge: پورتی که کاربر می‌زند (مثال 443 یا 7000)
- روی Destination: پورت 8443 (برای اتصال Worker)

------------------------------------------------------------
1) نصب خودکار (روی هر دو سرور)
------------------------------------------------------------
این دستور را روی هر دو سرور جداگانه اجرا کنید:

bash -lc 'set -euo pipefail; REPO="https://github.com/YouseFMutE/Jai.git"; APP="$HOME/aegis-edge-relay"; SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update && $SUDO DEBIAN_FRONTEND=noninteractive apt-get install -y git curl ca-certificates build-essential whiptail pkg-config libssl-dev; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git curl ca-certificates gcc gcc-c++ make newt pkgconf-pkg-config openssl-devel; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git curl ca-certificates gcc gcc-c++ make newt pkgconfig openssl-devel; elif command -v pacman >/dev/null 2>&1; then $SUDO pacman -Sy --noconfirm git curl ca-certificates base-devel libnewt pkgconf openssl; else echo "Unsupported distro"; exit 1; fi; rm -rf "$APP"; git clone "$REPO" "$APP"; cd "$APP"; chmod +x deploy.sh; ./deploy.sh'

نکته:
- این اسکریپت Rust را نصب می‌کند، پروژه را build می‌کند، منوی نصب می‌دهد و systemd service می‌سازد.

------------------------------------------------------------
2) تنظیمات نصب روی Bridge (Server-IR)
------------------------------------------------------------
وقتی deploy.sh اجرا شد، این مقادیر را وارد کنید:

- Current Node Role: Iran Bridge
- Clean Anycast IP: مثلا 1.1.1.1
- Custom Domain/Host: دامنه Worker شما (مثلا relay.example.fun)
- Auth Secret Key Mode:
  - پیشنهاد: Generate Random
  - کلید نمایش داده‌شده را ذخیره کنید (برای سرور Destination لازم است)
- Target Port: پورتی که کاربر وصل می‌شود (مثلا 443 یا 7000)

بعد از نصب:

sudo systemctl status aegis-edge-relay --no-pager
ss -lntp | grep -E ':443|:7000'

------------------------------------------------------------
3) تنظیمات نصب روی Destination (Server-OUT)
------------------------------------------------------------
وقتی deploy.sh اجرا شد، این مقادیر را وارد کنید:

- Current Node Role: Foreign Exit
- Custom Domain/Host: یک مقدار معتبر (در این mode کاربرد مسیر اصلی ندارد)
- Auth Secret Key Mode: Manual Entry
- Auth Secret Key: دقیقا همان کلید Bridge
- Target Port: پورت inbound واقعی V2Ray روی همین سرور (مثلا 10000)

بعد از نصب:

sudo systemctl status aegis-edge-relay --no-pager
ss -lntp | grep ':8443'
ss -lntp | grep ':10000'

------------------------------------------------------------
4) ساخت Worker در Cloudflare (Dashboard)
------------------------------------------------------------
1) وارد Cloudflare شوید.
2) از منو: Workers & Pages -> Create Application -> Create Worker.
3) محتوای فایل زیر را کامل کپی کنید و در Worker جایگزین کنید:
- /home/USER/aegis-edge-relay/worker.js
(یا در همان سرور مسیر واقعی پروژه را باز کنید)
4) Deploy کنید.
5) در Worker Settings -> Variables این ENVها را بگذارید:
- AUTH_SECRET_KEY = همان کلید مشترک
- EXIT_NODE_HOST = آی‌پی عمومی Destination
- EXIT_NODE_PORT = 8443
- EXIT_NODE_SCHEME = ws
6) روی دامنه Route بگذارید، مثال:
- relay.example.fun/*
7) مطمئن شوید DNS دامنه روی Proxy (ابر نارنجی) باشد.

------------------------------------------------------------
5) تنظیم x-ui / V2Ray روی Destination
------------------------------------------------------------
1) در x-ui یک inbound فعال داشته باشید (مثلا پورت 10000).
2) پروتکل باید TCP-based باشد.
3) در deploy.sh روی Destination، Target Port باید همان پورت inbound باشد.

------------------------------------------------------------
6) تنظیم V2Ray Client کاربر
------------------------------------------------------------
در کانفیگ کاربر فقط این دو مورد را به Bridge تغییر دهید:
- address = آی‌پی یا دامنه Bridge
- port = همان Target Port Bridge (مثلا 443 یا 7000)

بقیه پارامترها را مثل کانفیگ اصلی inbound نگه دارید:
- uuid / id
- security
- network/path/sni (اگر در پروفایل شما وجود دارد)

------------------------------------------------------------
7) تست مرحله‌به‌مرحله
------------------------------------------------------------
A) بررسی سرویس روی Bridge:

sudo systemctl status aegis-edge-relay --no-pager
sudo journalctl -u aegis-edge-relay -n 80 --no-pager

B) بررسی سرویس روی Destination:

sudo systemctl status aegis-edge-relay --no-pager
sudo journalctl -u aegis-edge-relay -n 80 --no-pager

C) بررسی پورت‌ها:

# Bridge
ss -lntp | grep -E ':443|:7000'

# Destination
ss -lntp | grep ':8443'
ss -lntp | grep ':10000'

D) تست نهایی با V2Ray Client:
- کاربر باید با address/port جدید Bridge وصل شود.
- اگر وصل شد و ترافیک رد شد، مسیر کامل درست است.

------------------------------------------------------------
8) خطاهای رایج و رفع سریع
------------------------------------------------------------
1) Unauthorized در لاگ‌ها:
- Secret بین Bridge / Worker / Destination یکسان نیست.

2) وصل می‌شود ولی دیتا رد نمی‌شود:
- Target Port روی Destination اشتباه است (باید پورت inbound واقعی باشد).

3) کاربر اصلا به Bridge وصل نمی‌شود:
- فایروال یا Security Group پورت Bridge را بسته است.

4) Worker به Destination نمی‌رسد:
- EXIT_NODE_HOST یا EXIT_NODE_PORT اشتباه است.
- پورت 8443 روی Destination بسته است.

5) بعد از ریبوت سرویس بالا نیامد:

sudo systemctl daemon-reload
sudo systemctl enable --now aegis-edge-relay

------------------------------------------------------------
9) دستورات نگهداری
------------------------------------------------------------
ری‌استارت سرویس:

sudo systemctl restart aegis-edge-relay

مشاهده لاگ زنده:

sudo journalctl -u aegis-edge-relay -f

آپدیت از گیتهاب و بیلد مجدد:

cd "$HOME/aegis-edge-relay"
git pull
source "$HOME/.cargo/env"
cargo build --release
sudo systemctl restart aegis-edge-relay

پایان
-----
اگر همه مراحل بالا را دقیق انجام دهید، مسیر شما به‌صورت عملیاتی این خواهد بود:
User -> Bridge -> Cloudflare Worker -> Destination -> V2Ray Inbound
