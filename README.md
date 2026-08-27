# vanish

Your desktop locks itself when you are not in front of it.

> **Alpha — v0.1.0-alpha.** Running on exactly one machine, mine. Config keys,
> defaults and the control socket can all change without notice.
>
> **Provenance:** this was written by Claude Code, from my design brief and
> against my hardware. It has unit tests, and every live path in it was
> exercised on a real session before publishing — but no human has audited it
> line by line. It holds a secret, opens a listening socket, and can lock you
> out of your own desktop. Read it before you trust it, and run
> `vanish run --dry-run` before you arm it.

Three ways of noticing you left, one decision about what to do:

* **an anchor** — a USB stick on your keyring, or the far end of a magnetic
  breakaway cable. Pull it, the screen locks.
* **a beacon** — the headset you are already wearing. Walk out of bluetooth
  range with it in your ears and the screen locks.
* **a webhook** — anything else that can make an HTTP request: a camera that
  stopped seeing a face, a motion sensor, a phone geofence, a line in a script.

It only ever locks. That is the whole point, and it is what makes it different
from [`deadhand`](https://github.com/one-more-refactor/deadhand), which watches
the same kind of USB anchor and cuts the power instead. A lock is cheap to be
wrong about, so vanish can afford a trigger that guesses — and gives you a
countdown to cancel it in.

```
$ vanish status
armed
  anchor   present (/devices/pci0000:00/.../usb3/3-2/3-2.4)
  beacon   connected, -61 dBm, in ear — armed
```

---

## How it locks

By default, `loginctl lock-session` on whichever session is live on seat0 — not
a locker binary. On a normal setup that raises systemd's `Lock` signal, which
your idle daemon already listens for, which runs the one locker you already
have. Three benefits fall out of that:

* the idle path, the suspend path and vanish all end in the same place;
* locking twice is a no-op, so vanish never has to track whether the screen is
  already locked;
* nothing here has to know that hyprlock, swaylock or gtklock exists.

If you run your locker directly instead, set `lock_command` and vanish will use
that.

It deliberately does not use a bare `loginctl lock-session` with no argument: a
user service lives in systemd's manager session, which has no seat and cannot
be locked. That call exits 0 and leaves your desktop wide open.

## The grace window

Every trigger starts a countdown rather than locking. During it:

* plugging the anchor back in cancels it;
* the headset reconnecting cancels it;
* `vanish pause 30m` cancels it and stops the next one;
* if [notchd](https://github.com/one-more-refactor/notchd) is running, the
  countdown is on screen.

Only the source that started a countdown can cancel it. Your headset coming
back says nothing about a USB stick that is still on the floor.

After a lock there is a cooldown (default 60s). Without one, a bluetooth link
that flaps re-locks the session while you are typing your password into it.

## The beacon, honestly

bluez only publishes `RSSI` for objects it hears **advertising**. A headset
connected over classic bluetooth does not advertise under the address it is
paired as — AirPods advertise from a randomised BLE address that rotates and
has no link back to the paired object. So "the signal strength of my connected
headset" is not a thing that exists, and the primary signal is `Connected`
going false when the link supervision timeout expires after you walk away.

That is a few seconds later than a distance threshold would be. It is the right
trade for something that locks your screen: late and certain beats early and
wrong. The distance path is still there (`away_rssi`), matched by advertisement
payload shape; it ships off because the correct threshold is a property of your
walls. `vanish rssi` prints live readings — stand where "away" begins and use
what you see.

### Why in-ear gates the whole thing

Taking the buds out and dropping them in the case disconnects them, which looks
exactly like walking out of the building. Without a guard, vanish would lock
your screen while you are sitting in front of it.

So the beacon counts as evidence only while it has recently been **worn**. Buds
on the desk are not a beacon; they are just bluetooth devices, and vanish
ignores them until they go back in your ears. In-ear state comes from the same
unencrypted Apple proximity advertisement, so it needs no pairing and no
accessory channel.

Non-Apple headsets have no in-ear bit to read. Set `require_in_ear = false` and
accept that "in the case" and "in another room" look the same — or use the
anchor, which never has that problem.

## The anchor

Two independent detectors, because one path is one point of failure: a raw
`NETLINK_KOBJECT_UEVENT` socket for the instant `remove@`, and a sysfs poll as
a backstop. Neither needs root and neither needs udevd.

Identify the device by unplugging it, not by reading `lsusb` and picking a
line — that is how people end up with their own keyboard as the anchor:

```
$ vanish learn
Unplug the device you want to use as the anchor, then plug it back in.

# Yubico YubiKey FIDO
[anchor]
enabled = true
vendor_id = "1050"
product_id = "0402"
serial = ""
```

An anchor that is already missing when vanish starts does **not** fire. deadhand's
equivalent does, because there the trigger cuts power on a machine that may have
been tampered with. Here it would mean logging in and being locked straight back
out by a daemon that started before you plugged anything in.

## The webhook

```
curl -X POST -H "Authorization: Bearer $TOKEN" \
     -d '{"reason":"no face for 30s"}' http://127.0.0.1:9911/lock
```

Hand-rolled HTTP/1.1, one route, one header. Two rules it will not bend on:
loopback by default — an authenticated-by-nobody port that locks a workstation
is a denial-of-service someone else gets to run — and one identical 404 for
every failure, so wrong path, wrong token and wrong method are
indistinguishable from outside.

`reason` is optional and lands in the journal, so the log says which sensor
fired rather than just "webhook".

## Install

```
cargo build --release
install -Dm755 target/release/vanish ~/.local/bin/vanish
mkdir -p ~/.config/vanish && cp vanish.toml.example ~/.config/vanish/config.toml
$EDITOR ~/.config/vanish/config.toml

install -Dm644 contrib/vanish.service ~/.config/systemd/user/vanish.service
systemctl --user daemon-reload && systemctl --user enable --now vanish
```

Run `vanish run --dry-run` first. It does everything except lock, and says what
it would have done.

## Commands

```
vanish [run] [--dry-run]     run the daemon
vanish status                what it currently believes
vanish pause [30m]           stop locking for a while
vanish resume                undo a pause
vanish lock                  lock right now
vanish learn                 identify a USB anchor by unplugging it
vanish rssi [ADDRESS]        live beacon readings, for tuning away_rssi
vanish gen-token             print a webhook token
```

`pause` exists because "stop it for the next half hour" needs to be faster than
editing a config file, and because `systemctl --user stop` is how you forget to
turn a security tool back on.

## A note on using a YubiKey as the anchor

It works, and there is a nice property to it if your locker authenticates
against the key: pulling it locks the session *and* drops the unlock path back
to your password, because the token it wanted is now in your pocket.

Check that your PAM stack actually falls back before you rely on it. A
`pam_u2f` line that is `required` rather than guarded by a presence check will
lock you out of your own machine when you pull the anchor.

## License

GPL-3.0-or-later.
