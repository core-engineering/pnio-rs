//! Write the GSDML of the sample configuration (16 REAL + 32 BOOL per direction) and print
//! the resulting controller address map.
use clap::Parser;
use pnio::config::{DeviceConfig, Direction, Slot};
use pnio::data::FieldType::*;
use pnio::gsdml::{file_name, render, GsdmlMeta};
use pnio::im::Im0;

#[derive(Parser)]
struct Args {
    /// Output directory
    #[arg(long, default_value = ".")]
    out: std::path::PathBuf,
    /// Station name
    #[arg(long, default_value = "pnio-dev")]
    station: String,
    /// Vendor ID (development default, not a PI-assigned ID)
    #[arg(long, default_value_t = 0xFFFF)]
    vendor_id: u16,
    #[arg(long, default_value_t = 0x0001)]
    device_id: u16,
    /// MinDeviceInterval in 31.25 us units: 32 = 1 ms, 16 = 500 us
    #[arg(long, default_value_t = 32)]
    interval: u16,
}

fn sample_config(a: &Args) -> DeviceConfig {
    DeviceConfig::builder(&a.station)
        .station_type("pnio sample device")
        .identity(a.vendor_id, a.device_id)
        .min_device_interval(a.interval)
        .input(Slot(1), &[Real; 16])
        .input(Slot(2), &[Bool; 32])
        .output(Slot(3), &[Real; 16])
        .output(Slot(4), &[Bool; 32])
        .im0(Im0 {
            order_id: "PNIO-SAMPLE".into(),
            ..Im0::default()
        })
        .build()
        .expect("sample config is valid")
}

fn main() {
    let a = Args::parse();
    let cfg = sample_config(&a);
    let meta = GsdmlMeta {
        vendor_name: "Core Engineering".into(),
        product_family: "pnio".into(),
        info_text: "pnio sample device: 16 REAL + 32 BOOL per direction".into(),
        date: (2026, 8, 29),
    };
    let path = a.out.join(file_name(&meta));
    std::fs::write(&path, render(&cfg, &meta)).expect("write gsdml");
    println!("wrote {}", path.display());
    println!("slot  dir     bytes  fields");
    let (mut ib, mut qb) = (0u32, 0u32);
    for s in cfg.submodules() {
        for (dir, len, base, tag) in [
            (
                Direction::Input,
                cfg.input_len(s.slot).unwrap_or(0),
                &mut ib,
                "%IB",
            ),
            (
                Direction::Output,
                cfg.output_len(s.slot).unwrap_or(0),
                &mut qb,
                "%QB",
            ),
        ] {
            if len == 0 {
                continue;
            }
            let n = cfg.fields(s.slot, dir).map(|f| f.len()).unwrap_or(0);
            println!(
                "{:<5} {:<7} {:<6} {} fields -> {tag}{}..{}",
                s.slot.0,
                format!("{dir:?}"),
                len,
                n,
                *base,
                *base + len as u32 - 1
            );
            *base += len as u32;
        }
    }
    println!(
        "(controller addresses assume TIA packs the modules in slot order from 0; check the device view)"
    );
}
