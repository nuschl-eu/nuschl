#!/bin/sh
# SPDX-License-Identifier: EUPL-1.2
# PID 1 of the Ubuntu install test, on Ubuntu's kernel: loads Ubuntu's
# AppArmor policy, restricts unprivileged user namespaces as Ubuntu does by
# default, and runs install-test.sh with its own arguments.
set -x
export PATH=/usr/sbin:/usr/bin
mount -o remount,rw /
mount -t securityfs securityfs /sys/kernel/security
mount -t tmpfs tmpfs /run
mount -t tmpfs tmpfs /tmp
apparmor_parser --replace /etc/apparmor.d
sysctl -w kernel.apparmor_restrict_unprivileged_userns=1
# The restriction is in effect: a user namespace of an unconfined program
# gets no capabilities.
if runuser -u nobody -- unshare --user --map-root-user --mount mount -t tmpfs none /mnt; then
  echo "user namespace restriction inactive"
else
  sh /install-test.sh "$@"
fi
echo o > /proc/sysrq-trigger
sleep 10
