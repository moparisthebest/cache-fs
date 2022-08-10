cache-fs
--------

caching fs to use over immutable network filesystems

The use-case is something like your Steam Deck has a large directory of roms mounted over NFS, but if you disable wifi,
you want the roms you've already played still available.

This basically bind-mounts your NFS share over a path of your choosing, except it caches all file attributes/paths
forever, and copies the files you open to your cache directory. When you access that file again it doesn't access the
remote server at all.  So if your remote (say NFS) server is not available, you can still play the games.

If you add/change files to your remote, you must delete `/local/cache/dir/cache-fs.tree` and remount.

Usage
-----

This assumes you install the `cache-fs` binary as `mount.cachefs`, then you can use it in /etc/fstab etc
```
mount -t cachefs -o remote_dir=/remote/dir/to/cache /local/cache/dir /where/you/want/it/mounted
```

Or put it in /etc/fstab like:
```
/local/cache/dir /where/you/want/it/mounted cachefs defaults,ro,allow_other,remote_dir=/remote/dir/to/cache,nofail,_netdev 0     0
```

How to compile
--------------

If you have rust installed already, run the normal:
`cargo build --release`

If you don't, or if you need to compile for some ancient glibc, and have podman or docker, run:
`podman run --rm -v "$PWD":/usr/src/myapp -w /usr/src/myapp docker.io/library/rust:1.62.1 bash -c 'apt-get update && apt-get -y install libfuse-dev && cargo build --release && strip target/release/cache-fs'`

How to use it on the Steam Deck over NFS
---------------------------------------

There are many ways to set this up, personally I run `sudo systemctl start sshd` on the Deck and run all these commands
via ssh from another computer, but that's optional, this is how I did it.

(Optional): To speed first access up, you can pre-cache your filesystem on the NFS server, or from a computer with a faster
(perhaps wired) connection by running `cache-fs -c /path/to/server/roms/dir/`, this will create a file `/path/to/server/roms/dir/cache-fs.tree.zst`
which will be copied to the cache directory on first run instead of made by scanning the NFS share over Deck WiFi.

Switch to desktop mode, install [EmuDeck](https://www.emudeck.com/) following instructions from there, copy your compiled
`cache-fs` to `/home/deck/cache-fs` (I run `scp target/release/cache-fs deck@steamdeck:/home/deck/`) then run these
commands, you'll need to re-run them again after any SteamOS update:

```
# create the directories to be mounted
mkdir -p /home/deck/Emulation/{romsnfs,roms,roms-cache}

# type your password to get a root console
sudo -i

# link the executable so mount.cachefs works
ln -sf /home/deck/cache-fs /usr/bin/mount.cachefs
# disable readonly fs
steamos-readonly disable
# initialize the pacman keyring
pacman-key --init

# these next 3 commands shouldn't be necessary but are, probably a bug to report to Valve...
# this command will fail, press 'N' to not delete the package
pacman -Sy archlinux-keyring
# delete the signature file so we can install it anyway
rm /var/cache/pacman/pkg/archlinux-keyring-*.pkg.tar.zst.sig
# install the keys, press 'Y' to trust them
pacman -U /var/cache/pacman/pkg/archlinux-*.pkg.tar.zst

# finally install what we were after, nfs-utils that provide mount.nfs
pacman -S --overwrite '*' nfs-utils
```

Now add the following to your `/etc/fstab`, I use run the command `nano /etc/fstab`, obviously replace IP and share with yours:
```
192.168.1.1:/mnt/deck/roms /home/deck/Emulation/romsnfs  nfs  defaults,ro,soft,timeo=100,retrans=0,retry=0,nodev,noexec,nosuid,noatime,async,v4,noauto,nofail,x-systemd.automount,x-systemd.mount-timeout=10,x-systemd.requires=NetworkManager.service,x-systemd.idle-timeout=1min,_netdev 0     0

/home/deck/Emulation/roms-cache /home/deck/Emulation/roms cachefs defaults,ro,allow_other,remote_dir=/home/deck/Emulation/romsnfs,nofail,_netdev 0     0
```

Now you can reboot, or just run, (as root):
```
systemctl daemon-reload
systemctl start home-deck-Emulation-romsnfs.automount home-deck-Emulation-roms.mount
```

You will now see all of your roms (the same network tree) in `/home/deck/Emulation/romsnfs` and `/home/deck/Emulation/roms`,
but `/home/deck/Emulation/roms-cache` will be empty until the first file is accessed and it's copied there, start up
EmulationStation and enjoy!

How to use it a different way
-----------------------------

Send me other ways you use it, if your roms are accessible over http, running cache-fs over [rclone](https://rclone.org/commands/rclone_mount/)
might be a great way to go, what else?
