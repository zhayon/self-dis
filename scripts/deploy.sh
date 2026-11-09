#!/usr/bin/env bash
# Ставит или обновляет self-dis на Ubuntu по SSH.
# RTC_IP=203.0.113.10 SSH_PORT=2222 scripts/deploy.sh ubuntu@203.0.113.10 dist/self-dis
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: RTC_IP=<адрес для браузера> scripts/deploy.sh user@host dist/self-dis" >&2
    exit 1
fi

host=$1
bin=$2
rtc_ip=${RTC_IP:-}
ssh_port=${SSH_PORT:-22}

if [[ ! -f "$bin" ]]; then
    echo "нет файла $bin" >&2
    exit 1
fi

root=$(cd "$(dirname "$0")/.." && pwd)
scp -P "$ssh_port" "$bin" "$host:/tmp/self-dis.new"
scp -P "$ssh_port" "$root/deploy/self-dis.service" "$host:/tmp/self-dis.service"

ssh -p "$ssh_port" "$host" "bash -s -- $(printf '%q' "$rtc_ip")" << 'EOF'
set -euo pipefail
rtc_ip=$1
if [[ "$(id -u)" -eq 0 ]]; then
    sudo() { "$@"; }
fi

sudo cp /tmp/self-dis.new /usr/local/bin/self-dis.next
sudo chmod 755 /usr/local/bin/self-dis.next
sudo mv /usr/local/bin/self-dis.next /usr/local/bin/self-dis
sudo cp /tmp/self-dis.service /etc/systemd/system/self-dis.service
rm -f /tmp/self-dis.new /tmp/self-dis.service

if ! id self-dis >/dev/null 2>&1; then
    sudo useradd --system --home-dir /var/lib/self-dis --shell /usr/sbin/nologin self-dis
fi
sudo mkdir -p /var/lib/self-dis
sudo chown self-dis:self-dis /var/lib/self-dis

if [[ ! -f /etc/self-dis.env ]]; then
    if [[ -z "$rtc_ip" ]]; then
        rtc_ip=$(ip -4 route get 1.1.1.1 | awk '{for (i = 1; i <= NF; i++) if ($i == "src") { print $(i + 1); exit }}')
    fi
    printf 'LISTEN=0.0.0.0:8080\nRTC_UDP=0.0.0.0:9000\nRTC_IP=%s\n' "$rtc_ip" | sudo tee /etc/self-dis.env >/dev/null
fi

sudo systemctl daemon-reload
sudo systemctl enable self-dis
sudo systemctl restart self-dis
sudo systemctl --no-pager --full status self-dis
echo
echo "настройки:"
sudo cat /etc/self-dis.env
EOF
