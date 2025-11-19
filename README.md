# **pve-ct-to-vm-img**

🤖 **Note:** This tool was "vibecoded" with **Gemini 3.0**, a quick utility developed to solve a very specific problem I encountered managing a large number of virtual machines.  
⚠️ **Disclaimer:** This is a hack. Please do your own research and testing before using it in a production system.

## **🎯 Purpose**

**img-prepend** is a specialized Rust CLI tool to convert a raw Linux filesystem image (e.g., a container rootfs export) into a **bootable, GPT-partitioned virtual disk image.**  
I built this to automate converting dozens of Proxmox LXC container images into full KVM virtual machines, saving me from tedious manual partitioning steps every time. Since this might be useful for others migrating systems or building disk images, I've put it here.

## **✨ Key Functionality**

The tool handles the necessary disk geometry to make a raw filesystem image usable as a primary VM disk:

- **Prepends Padding:** Adds a specified amount of empty space (--pad-mib) at the beginning of the disk for the boot sectors and EFI partition. This space is created **sparsely** (zeroed but unallocated on disk, which is highly efficient on ZFS, Ext4, etc.).
- **GPT Creation:** Wraps the entire structure with a valid GPT (Primary and Backup headers).
- **EFI Partitioning (--esp):** Optionally creates a correctly sized and **FAT32-formatted EFI System Partition (ESP)** in the padded space.
- **Data Placement:** Places the input image data immediately after the padding/ESP.
- **Fixed GUIDs:** Enforces standard Partition GUIDs for easy automation of fstab and GRUB configuration.

## **📦 Usage**

### **Installation**

Ensure you have Rust and Cargo installed, then build the release binary:  
git clone \<repository-url\>  
cd img-prepend  
cargo build \--release

### **Example: Creating an EFI-Ready Disk**

Use the \--esp flag to create the full structure, including the boot partition.
```
./target/release/img-prepend \ 
    --input rootfs.raw \  
    --output vm-disk.raw \  
    --pad-mib 2048 \
    --esp
```

### **Helper Output**

The tool prints the recommended /etc/fstab entries needed to mount the partitions inside your resulting VM:  
Recommended /etc/fstab entries:
```
# <file system> <mount point> <type> <options> <dump> <pass>  
proc /proc proc defaults 0 0  
PARTLABEL=efiesp /boot/efi vfat umask=0077 0 1  
PARTUUID=1f64a68b-eb12-443b-a55e-5aad64c3b432 / ext4 errors=remount-ro 0 1
```
