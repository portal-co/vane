#!/bin/busybox sh
# Net gate guest init (PLAN_PI_IN_VM P1): configure eth0, fetch the banner
# and echo a string through the host tunnel, power off.
# Statically maps 10.0.2.100 -> the host echo server via --net-map.
export PATH=/bin:/sbin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null

echo "=== VANE NET GATE ==="
ip link set eth0 up || { echo "FAIL: no eth0 (virtio-net driver missing?)"; poweroff -f; }
ip addr add 10.0.2.15/24 dev eth0
ip addr show eth0

# Banner + echo through the tunnel (static map 10.0.2.100 -> 127.0.0.1).
( echo "vane-net-echo-ping"; sleep 5 ) | nc -w 30 10.0.2.100 17777 > /tmp/resp
cat /tmp/resp
grep -q "VANE-ECHO-BANNER" /tmp/resp && echo "=== VANE NET BANNER OK ===" || echo "FAIL: no banner"
grep -q "vane-net-echo-ping" /tmp/resp && echo "=== VANE NET ECHO OK ===" || echo "FAIL: no echo"
poweroff -f
