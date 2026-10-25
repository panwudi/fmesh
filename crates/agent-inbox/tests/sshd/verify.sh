#!/bin/sh
# Destructive setup is confined to a disposable Docker container, never the host.
set -eu
[ "${INBOX_SSHD_TEST_CONTAINER:-}" = '1' ] && [ -f /.dockerenv ] && [ "$(id -u)" = '0' ] || {
    printf '%s\n' 'Run only in the documented disposable Docker test container.' >&2
    exit 1
}
inbox_test_dir=$(mktemp -d /tmp/agent-inbox-sshd-test.XXXXXX)
inbox_test_pids=''
inbox_cleanup() {
    for inbox_pid in $inbox_test_pids; do kill "$inbox_pid" 2>/dev/null || :; done
    for inbox_pid in $inbox_test_pids; do wait "$inbox_pid" 2>/dev/null || :; done
    rm -rf "$inbox_test_dir" /tmp/agent-tunnel-write-check
}
trap inbox_cleanup EXIT HUP INT TERM
umask 077
ssh-keygen -q -t ed25519 -N '' -f "$inbox_test_dir/host"
ssh-keygen -q -t ed25519 -N '' -f "$inbox_test_dir/claude"
ssh-keygen -q -t ed25519 -N '' -f "$inbox_test_dir/codex"
ssh-keygen -q -t ed25519 -N '' -f "$inbox_test_dir/unregistered"
useradd --system --no-create-home --home-dir /var/lib/agent-tunnel --shell /usr/sbin/nologin agent-tunnel
install -d -o root -g root -m 0755 /var/lib/agent-tunnel /run/sshd
install -d -o agent-tunnel -g agent-tunnel -m 0700 /tmp/agent-tunnel-write-check
inbox_claude_pub=$(awk '{print $2}' "$inbox_test_dir/claude.pub")
inbox_codex_pub=$(awk '{print $2}' "$inbox_test_dir/codex.pub")
sed -e "s|REPLACE_WITH_CLAUDE_SESSION_PUBLIC_KEY|$inbox_claude_pub|" \
    -e "s|REPLACE_WITH_CODEX_SESSION_PUBLIC_KEY|$inbox_codex_pub|" \
    /artifacts/deploy/agent-tunnel.authorized_keys.example > /etc/ssh/agent-tunnel.authorized_keys
chmod 0644 /etc/ssh/agent-tunnel.authorized_keys
awk '{print "restricted-local " $1 " " $2}' "$inbox_test_dir/host.pub" > "$inbox_test_dir/known_hosts"
cat > "$inbox_test_dir/sshd_config" <<EOF
Port 22221
ListenAddress 127.0.0.1
HostKey $inbox_test_dir/host
PidFile $inbox_test_dir/sshd.pid
UsePAM yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
PubkeyAuthentication yes
AllowUsers agent-tunnel
LogLevel ERROR
Include /artifacts/deploy/sshd-agent-tunnel.conf.example
EOF
/usr/sbin/sshd -t -f "$inbox_test_dir/sshd_config"
/usr/sbin/sshd -T -f "$inbox_test_dir/sshd_config" -C user=agent-tunnel,host=localhost,addr=127.0.0.1 > "$inbox_test_dir/effective"
grep -q '^maxsessions 0$' "$inbox_test_dir/effective"
grep -q '^forcecommand /usr/sbin/nologin$' "$inbox_test_dir/effective"
grep -q '^allowtcpforwarding yes$' "$inbox_test_dir/effective"
grep -q '^gatewayports no$' "$inbox_test_dir/effective"
/usr/sbin/sshd -D -e -f "$inbox_test_dir/sshd_config" > "$inbox_test_dir/server.log" 2>&1 &
inbox_test_pids="$inbox_test_pids $!"
inbox_http() {
    while :; do printf 'HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nokay\n' | nc -N -l 127.0.0.1 "$1" >/dev/null; done
}
inbox_http 8811 & inbox_test_pids="$inbox_test_pids $!"
inbox_http 8815 & inbox_test_pids="$inbox_test_pids $!"
inbox_http 8816 & inbox_test_pids="$inbox_test_pids $!"
export INBOX_TEST_KNOWN_HOSTS="$inbox_test_dir/known_hosts"
cat > "$inbox_test_dir/client" <<'EOF'
#!/bin/sh
inbox_key=$1; shift
exec ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes \
    -o StrictHostKeyChecking=yes -o HostKeyAlias=restricted-local \
    -o "UserKnownHostsFile=$INBOX_TEST_KNOWN_HOSTS" -o "ConnectTimeout=${INBOX_TEST_CLIENT_CONNECT_TIMEOUT:-2}" \
    -i "$inbox_key" -p 22221 -l agent-tunnel "$@"
EOF
chmod 0700 "$inbox_test_dir/client"
# Authentication and forwarding work even though the account has a nologin shell.
"$inbox_test_dir/client" "$inbox_test_dir/claude" -M -N -T -n -S "$inbox_test_dir/claude-control" \
    -o ExitOnForwardFailure=yes -R 127.0.0.1:8817:127.0.0.1:8815 \
    -L 127.0.0.1:8821:127.0.0.1:8811 127.0.0.1 > "$inbox_test_dir/client.log" 2>&1 &
inbox_test_pids="$inbox_test_pids $!"
inbox_ready=0
for inbox_try in 1 2 3 4 5 6 7 8 9 10; do
    if curl --silent --fail --max-time 1 http://127.0.0.1:8821/ > /dev/null; then inbox_ready=1; break; fi
    sleep 0.2
done
[ "$inbox_ready" = '1' ] || { cat "$inbox_test_dir/client.log" >&2; exit 1; }
curl --silent --fail --max-time 2 http://127.0.0.1:8817/ > /dev/null
inbox_expect_rejected() {
    inbox_status=0
    timeout 5 "$@" > "$inbox_test_dir/rejected.log" 2>&1 || inbox_status=$?
    [ "$inbox_status" -ne 0 ] && [ "$inbox_status" -ne 124 ] || {
        printf '%s\n' 'An operation was allowed or hung instead of being rejected.' >&2; exit 1;
    }
}
# A stale -R must not kill the replacement master or its useful -L. Restricted
# nologin/MaxSessions=0 permits these control requests without opening a shell.
cat > "$inbox_test_dir/supervisor.env" <<EOF
INBOX_SSH_USER='agent-tunnel'
INBOX_BIN='/artifacts/agent-inbox'
INBOX_SSH_CONFIG='/dev/null'
INBOX_SSH_KEY='$inbox_test_dir/claude'
INBOX_KNOWN_HOSTS='$inbox_test_dir/known_hosts'
INBOX_HOST_KEY_ALIAS='restricted-local'
INBOX_DIAL_LOCK_FILE='$inbox_test_dir/dial.lock'
INBOX_INTR_HOST='127.0.0.1'
INBOX_INTR_PORT=22221
INBOX_PUBLIC_HOST='127.0.0.1'
INBOX_PUBLIC_PORT=22221
INBOX_LOCAL_INBOX_PORT=8815
INBOX_REVERSE_PORT=8817
INBOX_LOCAL_SEND_PORT=8825
INBOX_HUB_PORT=8811
EOF
sh /artifacts/deploy/agent-inbox-tunnels.sh --config "$inbox_test_dir/supervisor.env" \
    > "$inbox_test_dir/replacement.log" 2>&1 &
inbox_replacement_pid=$!
inbox_test_pids="$inbox_test_pids $inbox_replacement_pid"
inbox_ready=0
for inbox_try in 1 2 3 4 5 6 7 8 9 10; do
    if curl --silent --fail --max-time 1 http://127.0.0.1:8825/ >/dev/null; then inbox_ready=1; break; fi
    sleep 0.2
done
[ "$inbox_ready" = '1' ] || { cat "$inbox_test_dir/replacement.log" >&2; exit 1; }
inbox_ready=0
for inbox_try in $(seq 1 40); do
    if [ "$(grep -c 'reverse forward unavailable' "$inbox_test_dir/replacement.log")" -ge 2 ]; then inbox_ready=1; break; fi
    sleep 0.2
done
[ "$inbox_ready" = '1' ] || { cat "$inbox_test_dir/replacement.log" >&2; exit 1; }
kill -0 "$inbox_replacement_pid"
curl --silent --fail --max-time 2 http://127.0.0.1:8825/ >/dev/null
ssh -F /dev/null -S "$inbox_test_dir/claude-control" -O cancel \
    -R 127.0.0.1:8817:127.0.0.1:8815 127.0.0.1
inbox_ready=0
for inbox_try in $(seq 1 40); do
    if grep -q 'both loopback forwards ready on existing SSH' "$inbox_test_dir/replacement.log"; then inbox_ready=1; break; fi
    sleep 0.2
done
[ "$inbox_ready" = '1' ] || { cat "$inbox_test_dir/replacement.log" >&2; exit 1; }
[ "$(grep -c 'connecting local hub forward' "$inbox_test_dir/replacement.log")" -eq 1 ]
! grep -q 'SSH ended' "$inbox_test_dir/replacement.log"
kill -0 "$inbox_replacement_pid"
curl --silent --fail --max-time 2 http://127.0.0.1:8817/ >/dev/null
curl --silent --fail --max-time 2 http://127.0.0.1:8825/ >/dev/null
printf '%s\n' 'Lingering reverse port: replacement master/-L survived repeated failures; -R attached on the same connection after release.'
# The other session's reverse port, a non-hub destination, shell and SFTP are denied.
inbox_expect_rejected "$inbox_test_dir/client" "$inbox_test_dir/claude" -N -T -n \
    -o ExitOnForwardFailure=yes -R 127.0.0.1:8819:127.0.0.1:8815 127.0.0.1
inbox_expect_rejected "$inbox_test_dir/client" "$inbox_test_dir/claude" -W 127.0.0.1:8815 127.0.0.1
inbox_expect_rejected "$inbox_test_dir/client" "$inbox_test_dir/claude" 127.0.0.1 'touch /tmp/agent-tunnel-write-check/compromised'
[ ! -e /tmp/agent-tunnel-write-check/compromised ]
inbox_expect_rejected "$inbox_test_dir/client" "$inbox_test_dir/claude" -s 127.0.0.1 sftp
inbox_expect_rejected "$inbox_test_dir/client" "$inbox_test_dir/unregistered" -N -T -n 127.0.0.1
# The second independent key can use its own reverse port, proving it was free.
"$inbox_test_dir/client" "$inbox_test_dir/codex" -N -T -n -o ExitOnForwardFailure=yes \
    -R 127.0.0.1:8819:127.0.0.1:8816 -L 127.0.0.1:8823:127.0.0.1:8811 \
    127.0.0.1 > "$inbox_test_dir/client2.log" 2>&1 &
inbox_test_pids="$inbox_test_pids $!"
inbox_ready=0
for inbox_try in 1 2 3 4 5 6 7 8 9 10; do
    if curl --silent --fail --max-time 1 http://127.0.0.1:8823/ >/dev/null; then inbox_ready=1; break; fi
    sleep 0.2
done
[ "$inbox_ready" = '1' ] || { cat "$inbox_test_dir/client2.log" >&2; exit 1; }
curl --silent --fail --max-time 2 http://127.0.0.1:8819/ >/dev/null
# The deployed route selector uses local facts; only the outer SSH authenticates.
cat > "$inbox_test_dir/tunnel.env" <<EOF
INBOX_SSH_USER='agent-tunnel'
INBOX_BIN='/artifacts/agent-inbox'
INBOX_SSH_KEY='$inbox_test_dir/claude'
INBOX_KNOWN_HOSTS='$inbox_test_dir/known_hosts'
INBOX_HOST_KEY_ALIAS='restricted-local'
INBOX_INTR_HOST='127.0.0.1'
INBOX_INTR_PORT=22222
INBOX_PUBLIC_HOST='127.0.0.1'
INBOX_PUBLIC_PORT=22221
INBOX_INTR_PREFIX='192.0.2.0/24'
EOF
export INBOX_TEST_CLIENT_CONNECT_TIMEOUT=10
"$inbox_test_dir/client" "$inbox_test_dir/claude" -M -N -T -n -S "$inbox_test_dir/proxy-control" \
    -o "ProxyCommand=sh /artifacts/deploy/hub-ssh-proxy.sh --config $inbox_test_dir/tunnel.env" \
    127.0.0.1 > "$inbox_test_dir/proxy.log" 2>&1 &
inbox_test_pids="$inbox_test_pids $!"
inbox_ready=0
for inbox_try in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    if [ -S "$inbox_test_dir/proxy-control" ] && \
        "$inbox_test_dir/client" "$inbox_test_dir/claude" -S "$inbox_test_dir/proxy-control" -O check 127.0.0.1 >/dev/null 2>&1; then
        inbox_ready=1; break
    fi
    sleep 0.2
done
[ "$inbox_ready" = '1' ] || { cat "$inbox_test_dir/proxy.log" >&2; exit 1; }
printf '%s\n' 'Restricted nologin account: -N/-R/-L, exact per-key limits, denied commands/SFTP, and local-facts route selection verified.'
