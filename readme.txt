Aegis Edge Relay - Quick Setup (Bridge + Destination + Cloudflare Worker)
======================================================================

هدف
----
مسیر سرویس به این شکل است:
User Client -> Bridge Node -> Cloudflare Worker -> Destination Node -> Local Service (مثلا Xray inbound)

نقش‌ها
------
- Bridge: سروری که کاربر مستقیم به آن وصل می‌شود.
- Destination: سروری که سرویس نهایی (مثلا Xray inbound) روی آن است.


1) نصب یک‌خطی (روی هر دو سرور جداگانه)
---------------------------------------
این دستور پیش‌نیازها را نصب می‌کند، ریپو را دانلود می‌کند، build می‌گیرد و setup منویی را اجرا می‌کند:

bash -lc 'set -euo pipefail; REPO="https://github.com/YouseFMutE/Jai.git"; APP="$HOME/aegis-edge-relay"; SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update && $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y git curl ca-certificates build-essential whiptail pkg-config libssl-dev; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y git curl ca-certificates gcc gcc-c++ make newt pkgconf-pkg-config openssl-devel; elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y git curl ca-certificates gcc gcc-c++ make newt pkgconfig openssl-devel; elif command -v pacman >/dev/null 2>&1; then $SUDO pacman -Sy --noconfirm git curl ca-certificates base-devel libnewt pkgconf openssl; else echo "Unsupported distro"; exit 1; fi; rm -rf "$APP"; git clone "$REPO" "$APP"; cd "$APP"; chmod +x deploy.sh; ./deploy.sh'


2) مقادیر منو برای Bridge
-------------------------
در deploy.sh این‌ها را وارد کنید:
- Current Node Role: Iran Bridge
- Clean Anycast IP: یک IPv4 تمیز Cloudflare (مثلا 172.64.152.23)
- Custom Domain/Host: دامنه Worker (مثلا edge.example.com)
- Auth Secret Key Mode: Generate Random (یا Manual)
- Target Port: پورتی که کاربر باید بزند (مثلا 1818 یا 443)

بعد از نصب روی Bridge:
- sudo systemctl status aegis-edge-relay --no-pager
- ss -lntp | grep ':1818'    # یا پورت انتخابی شما
- curl -fsS http://127.0.0.1:19090/healthz


3) مقادیر منو برای Destination
------------------------------
در deploy.sh این‌ها را وارد کنید:
- Current Node Role: Foreign Exit
- Custom Domain/Host: یک مقدار معتبر (در این mode حیاتی نیست)
- Auth Secret Key Mode: Manual Entry
- Auth Secret Key: دقیقا همان مقدار Bridge/Worker
- Target Port: پورت سرویس local (مثلا Xray inbound: 1818)

بعد از نصب روی Destination:
- sudo systemctl status aegis-edge-relay --no-pager
- ss -lntp | grep ':8443'
- ss -lntp | grep ':1818'    # یا پورت سرویس local شما
- curl -fsS http://127.0.0.1:19091/healthz


4) تنظیم Cloudflare Worker
--------------------------
1. Dashboard -> Workers & Pages -> Create Worker
2. محتوای فایل worker.js را کامل Paste کنید.
3. Deploy کنید.
4. Worker Variables را تنظیم کنید:
   - AUTH_SECRET_KEY = همان کلید مشترک
   - EXIT_NODE_HOST = IP عمومی Destination
   - EXIT_NODE_PORT = 8443
   - EXIT_NODE_SCHEME = ws
5. Route تنظیم کنید (پیشنهاد): edge.example.com/*

تست سلامت Worker:
- curl -s https://edge.example.com/healthz
- curl -s https://edge.example.com/readyz


5) تست کامل مسیر
----------------
A) تست Upgrade از Bridge به Worker:
- SECRET=$(sudo sed -n 's/^AUTH_SECRET_KEY="\(.*\)"/\1/p' /etc/aegis-edge-relay/aegis-edge-relay.env)
- curl -sk --http1.1 --resolve edge.example.com:443:172.64.152.23 \
  "https://edge.example.com/relay" \
  -H "Connection: Upgrade" -H "Upgrade: websocket" \
  -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" -H "Sec-WebSocket-Version: 13" \
  -H "Auth-Secret-Key: $SECRET" -o /dev/null -w "%{http_code}\n"

خروجی باید 101 باشد.

B) تست لاگ زنده
- Bridge: sudo journalctl -u aegis-edge-relay -f
- Destination: sudo journalctl -u aegis-edge-relay -f

C) در صورت مشکل، Packet Trace کوتاه
- Bridge: sudo timeout 20 tcpdump -ni any 'tcp port 1818'
- Destination: sudo timeout 20 tcpdump -A -s 0 -ni any 'tcp port 8443'
- Destination local: sudo timeout 20 tcpdump -ni lo 'tcp port 1818'


6) نکات مهم پورت
----------------
- پورت 8443 بین Worker و Destination استفاده می‌شود.
- پورت Bridge (مثلا 1818 یا 443) همان پورتی است که کاربر به آن وصل می‌شود.
- پورت local service در Destination همان Target Port نصب Destination است.


7) دستورات نگهداری
------------------
- ری‌استارت: sudo systemctl restart aegis-edge-relay
- لاگ زنده: sudo journalctl -u aegis-edge-relay -f
- نمایش ExecStart: sudo systemctl cat aegis-edge-relay | grep ExecStart

آپدیت:
- cd $HOME/aegis-edge-relay
- git pull
- source "$HOME/.cargo/env"
- cargo build --release
- sudo systemctl restart aegis-edge-relay
