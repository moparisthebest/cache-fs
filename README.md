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
mount -t cachefs -o cache_dir=/local/cache/dir /remote/dir/to/cache /where/you/want/it/mounted
```
