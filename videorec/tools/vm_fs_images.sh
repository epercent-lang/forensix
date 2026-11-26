#!/bin/bash
# Builds test images for file systems the host kernel cannot mount (HFS+,
# JFS, ReiserFS, APFS) by running the Ubuntu kernel with its modules in
# QEMU. Used once to make the *_deleted.img scenarios; kept as a record of
# how they were made.
#
# Needs: qemu-system-x86, busybox-static, cpio, zstd, hfsprogs, jfsutils,
# reiserfsprogs, apfsprogs, and the unpacked Ubuntu packages
# linux-image / linux-modules / linux-modules-extra (6.8.0-149-generic used)
# in $VM/ex. APFS additionally needs apfs.ko built from the apfs-dkms
# package (0.3.2 needs -D__NO_FORTIFY and generic_update_time()'s return
# value ignored in apfs_update_time() to work on kernel 6.8).
#
# usage: vm_fs_images.sh <vm-dir> <videos-dir> <out-dir>
set -e
VM=$1; VID=$2; OUT=$3
M=$VM/ex/lib/modules/6.8.0-149-generic/kernel
mkdir -p $VM/rd/{bin,mod,mnt,proc,sys,dev,work} $VM/pay
cp /usr/bin/busybox $VM/rd/bin/
for m in fs/reiserfs/reiserfs fs/hfsplus/hfsplus fs/nls/nls_ucs2_utils fs/jfs/jfs fs/nls/nls_utf8 lib/libcrc32c; do
  zstd -dqf $M/$m.ko.zst -o $VM/rd/mod/$(basename $m).ko
done
[ -f $VM/apfs.ko ] && cp $VM/apfs.ko $VM/rd/mod/
cat > $VM/rd/init <<'EOT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc p /proc; mount -t sysfs s /sys; mount -t devtmpfs d /dev
for m in nls_utf8 nls_ucs2_utils hfsplus jfs reiserfs libcrc32c apfs; do insmod /mod/$m.ko; done
FS=$(sed 's/.*vfs=\([a-z0-9]*\).*/\1/' /proc/cmdline)
OPT=$(sed -n 's/.*vopt=\([^ ]*\).*/\1/p' /proc/cmdline)
cd /work && tar xf /dev/vdb 2>/dev/null
mount -t $FS ${OPT:+-o $OPT} /dev/vda /mnt && echo MOUNT_OK || echo MOUNT_FAIL
sh /work/run.sh /mnt; echo RUN_RC=$?
cd /; umount /mnt && echo UMOUNT_OK
sync
poweroff -f
EOT
chmod +x $VM/rd/init
(cd $VM/rd && find . | cpio -o -H newc 2>/dev/null | gzip -1 > ../rd.gz)
cp $VID/{A,B,C,E}.mp4 $VM/pay/
cat > $VM/pay/run.sh <<'EOT'
M=$1
mkdir -p $M/record/ch01
for n in A B C E; do cp /work/$n.mp4 $M/record/ch01/$n.mp4; done
sync
for n in A C E; do rm -f $M/record/ch01/$n.mp4; done
EOT
run() { # fstype image [mount options]
  T=$(mktemp); tar cf $T -C $VM/pay .; truncate -s %512 $T; truncate -s +1M $T
  timeout 300 qemu-system-x86_64 -m 1024 -nographic -no-reboot \
    -kernel $VM/ex/boot/vmlinuz-6.8.0-149-generic -initrd $VM/rd.gz \
    -append "console=ttyS0 panic=-1 vfs=$1 ${3:+vopt=$3}" \
    -drive file="$2",format=raw,if=virtio -drive file=$T,format=raw,if=virtio 2>&1 |
    tr -d '\r' | grep -aE "MOUNT_|RUN_RC|UMOUNT"
  rm -f $T
}
mkdir -p $OUT
for f in hfsplus jfs reiserfs apfs; do rm -f $OUT/${f}_deleted.img; truncate -s 400M $OUT/${f}_deleted.img; done
mkfs.hfsplus $OUT/hfsplus_deleted.img >/dev/null
mkfs.jfs -q $OUT/jfs_deleted.img >/dev/null
mkreiserfs -q -f $OUT/reiserfs_deleted.img >/dev/null 2>&1
run hfsplus $OUT/hfsplus_deleted.img
run jfs $OUT/jfs_deleted.img
run reiserfs $OUT/reiserfs_deleted.img
if [ -f $VM/apfs.ko ]; then mkapfs $OUT/apfs_deleted.img && run apfs $OUT/apfs_deleted.img readwrite; fi
