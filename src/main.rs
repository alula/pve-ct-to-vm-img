use anyhow::{Context, Result};
use clap::Parser;
use fatfs::{FatType, FormatVolumeOptions, format_volume};
use fscommon::{BufStream, StreamSlice};
use gpt::{GptConfig, disk::LogicalBlockSize, partition_types};
use indicatif::{ProgressBar, ProgressStyle};
use std::convert::TryFrom;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use uuid::Uuid;

const PART_NAME_ESP: &str = "efiesp";
const PART_NAME_LINUX: &str = "linux";

// Specified Partition GUIDs
const PART_UUID_ESP: &str = "dd72ae4a-b812-4f7c-b59f-2bc5395d3aab";
const PART_UUID_LINUX: &str = "1f64a68b-eb12-443b-a55e-5aad64c3b432";

/// A tool to prepend empty space and wrap a raw filesystem image in a GPT partition table.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input raw filesystem image (e.g., rootfs.ext4)
    #[arg(short, long)]
    input: PathBuf,

    /// Output disk image path
    #[arg(short, long)]
    output: PathBuf,

    /// Amount of padding to prepend in MiB (Mebibytes) (default: 1024 = 1 GiB)
    #[arg(long, default_value_t = 1024)]
    pad_mib: u64,

    /// Add an EFI System Partition (ESP) entry in the padded space
    #[arg(long)]
    esp: bool,

    /// Logical Block Address size (512 or 4096)
    #[arg(long, default_value_t = 512)]
    lba: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Parse the constant UUIDs
    let uuid_esp = Uuid::from_str(PART_UUID_ESP).context("Failed to parse internal ESP UUID")?;
    let uuid_linux =
        Uuid::from_str(PART_UUID_LINUX).context("Failed to parse internal Linux UUID")?;

    // 1. Validate and Analyze Input
    let input_file = File::open(&args.input).context("Failed to open input file")?;
    let input_len = input_file.metadata()?.len();

    println!("Input image size: {} bytes", input_len);

    if args.lba != 512 && args.lba != 4096 {
        anyhow::bail!("LBA must be 512 or 4096");
    }

    // Check alignment
    if input_len % args.lba != 0 {
        println!(
            "Warning: Input image size is not aligned to LBA {}. It will be padded.",
            args.lba
        );
    }

    // 2. Calculate Geometry
    let pad_bytes = args.pad_mib * 1024 * 1024;

    // Calculate required LBAs
    let padding_lbas = pad_bytes / args.lba;
    let data_lbas = (input_len as f64 / args.lba as f64).ceil() as u64;

    // GPT requires 33 LBAs at the end for the backup header (usually 1 header + 32 entries)
    // We add a bit of safety margin (1 MiB) at the end to avoid edge cases
    let footer_lbas = (1024 * 1024) / args.lba;

    let total_lbas = padding_lbas + data_lbas + footer_lbas;
    let total_bytes = total_lbas * args.lba;

    println!("Configuration:");
    println!(
        "  Padding:      {} MiB ({} sectors)",
        args.pad_mib, padding_lbas
    );
    println!(
        "  Data Length:  {} bytes ({} sectors)",
        input_len, data_lbas
    );
    println!("  Footer Space: {} sectors", footer_lbas);
    println!("  Total Size:   {} bytes", total_bytes);

    // 3. Create Sparse Output File
    let output_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&args.output)
        .context("Failed to create output file")?;

    // Set the file length immediately (Sparse creation on Linux/ZFS)
    output_file
        .set_len(total_bytes)
        .context("Failed to set output file length")?;

    // 4. Write GPT Partition Table
    println!("Writing GPT Partition Table...");

    let lb_size = LogicalBlockSize::try_from(args.lba)
        .context("Unsupported LBA size (must be 512 or 4096)")?;

    // Create a new GPT configuration
    let gpt_config = GptConfig::new().writable(true).logical_block_size(lb_size);

    let mut disk = gpt_config
        .open(&args.output)
        .context("Failed to open output for GPT")?;

    // Initialize a new header
    disk.update_partitions(std::collections::BTreeMap::new())
        .context("Failed to initialize GPT header")?;

    // Define Data Partition Geometry
    // It always starts after the padding
    let data_start_lba = padding_lbas;
    let data_end_lba = data_start_lba + data_lbas - 1;

    let mut esp_region: Option<(u64, u64)> = None;
    if args.esp {
        // ESP Logic
        // Standard alignment: Start at 1 MiB
        let esp_start_lba = (1024 * 1024) / args.lba;
        // ESP ends right before the data partition starts
        let esp_end_lba = data_start_lba - 1;

        if esp_end_lba <= esp_start_lba {
            anyhow::bail!(
                "Padding is too small to contain an EFI System Partition. Increase --pad-mib."
            );
        }

        let esp_size_bytes = (esp_end_lba - esp_start_lba + 1) * args.lba;
        println!(
            "  -> Adding EFI System Partition (ESP) [{} MiB]",
            esp_size_bytes / 1024 / 1024
        );
        println!("     GUID: {}", uuid_esp);
        esp_region = Some((esp_start_lba, esp_size_bytes));

        // Add Partitions
        // Note: add_partition takes arguments (name, size_bytes, type_guid, flags, part_guid_opt)

        // 1. Add ESP
        let _ = disk
            .add_partition(PART_NAME_ESP, esp_size_bytes, partition_types::EFI, 0, None)
            .context("Failed to add ESP partition entry")?;

        // 2. Add Linux Data
        let _ = disk
            .add_partition(
                PART_NAME_LINUX,
                input_len,
                partition_types::LINUX_FS,
                0,
                None,
            )
            .context("Failed to add Data partition entry")?;

        println!("     Linux Data GUID: {}", uuid_linux);

        // Manually enforce exact LBAs
        let mut partitions = disk.partitions().clone();

        if let Some(p) = partitions.get_mut(&1) {
            p.first_lba = esp_start_lba;
            p.last_lba = esp_end_lba;
            p.part_guid = uuid_esp;
        }
        if let Some(p) = partitions.get_mut(&2) {
            p.first_lba = data_start_lba;
            p.last_lba = data_end_lba;
            p.part_guid = uuid_linux;
        }

        disk.update_partitions(partitions)
            .context("Failed to update partitions")?;
    } else {
        // Standard Logic (Just the data partition)
        let _ = disk
            .add_partition(
                PART_NAME_LINUX,
                input_len,
                partition_types::LINUX_FS,
                0,
                None,
            )
            .context("Failed to add partition entry")?;

        println!("     Linux Data GUID: {}", uuid_linux);

        // Manually enforce start at padding
        let mut partitions = disk.partitions().clone();
        let part = partitions.get_mut(&1).context("Partition 1 not found")?;
        part.first_lba = data_start_lba;
        part.last_lba = data_end_lba;
        part.part_guid = uuid_linux;

        disk.update_partitions(partitions)
            .context("Failed to update partitions")?;
    }

    disk.write()
        .context("Failed to write GPT changes to disk")?;

    if let Some((esp_start_lba, esp_size_bytes)) = esp_region {
        println!("Formatting ESP as FAT32...");
        format_esp_partition(
            &args.output,
            esp_start_lba * args.lba,
            esp_size_bytes,
            args.lba,
        )
        .context("Failed to format ESP partition")?;
    }

    // 5. Copy Data (Sparse Copy)
    println!("Copying filesystem image to offset...");

    let mut reader = BufReader::new(input_file);
    let mut writer = BufWriter::new(output_file);

    // Seek output to the start of the data partition
    writer
        .seek(SeekFrom::Start(data_start_lba * args.lba))
        .context("Failed to seek output file")?;

    // Setup progress bar
    let pb = ProgressBar::new(input_len);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
            )?
            .progress_chars("#>-"),
    );

    // buffer for copying
    let mut buffer = [0u8; 65536]; // Increased buffer size for potentially better throughput
    let mut copied = 0;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[0..read])?;
        copied += read as u64;
        pb.set_position(copied);
    }

    pb.finish_with_message("Copy complete");
    writer.flush()?;

    println!("Done! Created {}", args.output.display());
    if args.esp {
        println!("Note: ESP partition formatted as FAT32 with label EFI SYSTEM.");
    }

    // Print fstab entries
    println!("\nRecommended /etc/fstab entries:");
    println!("# <file system> <mount point> <type> <options> <dump> <pass>");
    println!("proc /proc proc defaults 0 0");
    if args.esp {
        println!("PARTLABEL={} /boot/efi vfat umask=0077 0 1", PART_NAME_ESP);
    }
    // Using PARTUUID because we are referencing the GPT Partition GUID, not the filesystem UUID.
    println!("PARTUUID={} / ext4 errors=remount-ro 0 1", uuid_linux);

    Ok(())
}

fn format_esp_partition(
    image_path: &Path,
    start_offset: u64,
    size_bytes: u64,
    lba_size: u64,
) -> Result<()> {
    if lba_size > u16::MAX as u64 {
        anyhow::bail!("LBA size {} exceeds FAT formatter limit", lba_size);
    }

    // Open the disk image file
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image_path)
        .with_context(|| format!("Failed to open {}", image_path.display()))?;

    // Create a slice view of the partition (end_offset is exclusive)
    let partition = StreamSlice::new(file, start_offset, start_offset + size_bytes)
        .context("Failed to create partition slice")?;

    // Wrap with buffering for optimized file access
    let mut buf_partition = BufStream::new(partition);

    // Format the partition as FAT32
    let options = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .volume_label(*b"EFI SYSTEM ")
        .bytes_per_sector(lba_size as u16);

    format_volume(&mut buf_partition, options).context("fatfs formatting failed")?;
    buf_partition
        .flush()
        .context("Failed to flush formatted ESP")?;

    Ok(())
}
