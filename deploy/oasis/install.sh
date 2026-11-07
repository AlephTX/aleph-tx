#!/usr/bin/env bash
# Run from the built source snapshot on oasis: deploy/oasis/install.sh RELEASE SHA256 COMMIT
set -euo pipefail
cd -- "$(dirname -- "$0")/../.."
release=${1:?Pass a release ID}
source_sha=${2:?Pass the source archive SHA-256}
commit=${3:?Pass the base commit}
[[ "$release" =~ ^[0-9]{8}-[a-z0-9]{12}$ && "$source_sha" =~ ^[a-f0-9]{64}$ && "$commit" =~ ^[a-f0-9]{12,40}$ ]]
root=/opt/aleph-tx
dest="$root/releases/$release"
# The SPA is built off-host (make dashboard-build) so the VM needs no Node toolchain.
if [[ ! -f dashboard/dist/index.html ]]; then
  printf 'Missing dashboard/dist; run make dashboard-build before snapshotting.\n' >&2
  exit 1
fi

live_state=$(systemctl show -p ActiveState --value aleph-tx-live.service)
case "$live_state" in
  inactive|failed) ;;
  *) printf 'Stop the live strategy cleanly before installing (state: %s).\n' "$live_state" >&2; exit 1 ;;
esac

for user in aleph-tx aleph-tx-trader; do
  if ! getent passwd "$user" >/dev/null; then
    sudo -n useradd --system --user-group --home-dir /nonexistent --shell /usr/sbin/nologin "$user"
  fi
done
sudo -n install -d -m 0755 "$root/releases" /etc/aleph-tx
sudo -n mkdir -m 0755 "$dest"
sudo -n install -d -m 0755 "$dest/bin" "$dest/lib" "$dest/dashboard" "$dest/examples"
sudo -n install -m 0755 feeder/feeder-app feeder/perp-capture \
  target/release/{aleph-tx,backpack_mm,cross_venue,edgex_mm,lighter_adaptive_mm,lighter_inventory_mm} "$dest/bin/"
sudo -n install -m 0644 src/native/lighter-signer-linux-amd64.so "$dest/lib/"
sudo -n install -m 0644 examples/{crypto_funding_paper,equity_basis_paper}.json "$dest/examples/"
sudo -n install -m 0644 deploy/oasis/{monitor,telemetry_store}.py "$dest/"
sudo -n cp -R --no-preserve=ownership dashboard/dist/. "$dest/dashboard/"
sudo -n chmod -R u=rwX,go=rX "$dest/dashboard"
printf '{"id":"%s","source_sha256":"%s","base_commit":"%s"}\n' "$release" "$source_sha" "$commit" |
  sudo -n tee "$dest/release.json" >/dev/null

# A dry replay checks the deployed executable and native loader before switching releases.
LD_LIBRARY_PATH="$dest/lib" "$dest/bin/cross_venue" perp-replay "$dest/examples/crypto_funding_paper.json" >/dev/null
if [[ ! -e /etc/aleph-tx/feeder.toml ]]; then
  sudo -n install -m 0644 deploy/oasis/feeder.toml /etc/aleph-tx/feeder.toml
fi
for unit in feeder monitor live; do
  sudo -n install -m 0644 "deploy/oasis/aleph-tx-$unit.service" /etc/systemd/system/
done
sudo -n install -m 0644 deploy/oasis/aleph-tx-telemetry.conf /etc/tmpfiles.d/aleph-tx-telemetry.conf
sudo -n systemd-tmpfiles --create /etc/tmpfiles.d/aleph-tx-telemetry.conf
# JavaScript rules need polkit >= 0.106 (Ubuntu 23.10+, Debian 13+).
sudo -n install -m 0644 deploy/oasis/50-aleph-tx-monitor.rules /etc/polkit-1/rules.d/
# Emergency-stop token: generated once, root-only, read by systemd via EnvironmentFile.
if ! sudo -n grep -q '^ALEPH_MONITOR_CONTROL_TOKEN=' /etc/aleph-tx/monitor.env 2>/dev/null; then
  token=$(python3 -c 'import secrets; print(secrets.token_urlsafe(24))')
  printf 'ALEPH_MONITOR_CONTROL_TOKEN=%s\n' "$token" | sudo -n tee -a /etc/aleph-tx/monitor.env >/dev/null
  unset token
fi
sudo -n chmod 0600 /etc/aleph-tx/monitor.env
sudo -n ln -s "$dest" "$root/.current-$release"
sudo -n mv -Tf "$root/.current-$release" "$root/current"
sudo -n systemctl daemon-reload
sudo -n systemctl enable aleph-tx-feeder.service aleph-tx-monitor.service
sudo -n systemctl restart aleph-tx-feeder.service aleph-tx-monitor.service
curl --fail --silent --show-error --retry 10 --retry-connrefused --retry-delay 1 --max-time 4 http://127.0.0.1:8765/api/status | jq .
