//! GSDML V2.4 rendering from a [`DeviceConfig`]: same object, same idents, same field
//! order as the code, so the file cannot drift from what the device answers (spec §5).
//! Text template; no XML library in the crate.

use crate::config::{DeviceConfig, Direction, FieldRef};
use crate::data::FieldType;
use std::fmt::Write;

/// Vendor/product texts and the date that go into the file (and its name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsdmlMeta {
    pub vendor_name: String,
    pub product_family: String,
    pub info_text: String,
    pub order_number: String,
    /// (year, month, day) — the GSDML release date, also the file-name suffix.
    pub date: (u16, u8, u8),
}

/// `GSDML-V2.4-<Vendor>-<Product>-<YYYYMMDD>.xml`, names stripped to `[A-Za-z0-9]`
/// (TIA rejects files that do not match this pattern).
pub fn file_name(meta: &GsdmlMeta) -> String {
    let clean = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
    };
    format!(
        "GSDML-V2.4-{}-{}-{:04}{:02}{:02}.xml",
        clean(&meta.vendor_name),
        clean(&meta.product_family),
        meta.date.0,
        meta.date.1,
        meta.date.2
    )
}

/// Escape the five XML specials for attribute/text content.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One `DataItem` (byte type) or one `Unsigned8 UseAsBits` group (<= 8 BOOLs of the
/// same byte), in declaration order.
enum Item<'a> {
    Scalar(FieldType, usize),
    Bits(Vec<(usize, &'a FieldRef)>),
}

fn items(fields: &[FieldRef]) -> Vec<Item<'_>> {
    let mut out = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        match f.ty {
            FieldType::Bool => match out.last_mut() {
                Some(Item::Bits(g)) if g.last().map(|(_, r)| r.byte) == Some(f.byte) => {
                    g.push((i, f))
                }
                _ => out.push(Item::Bits(vec![(i, f)])),
            },
            ty => out.push(Item::Scalar(ty, i)),
        }
    }
    out
}

fn data_type(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Bool => "Unsigned8",
        FieldType::Int => "Integer16",
        FieldType::Word => "Unsigned16",
        FieldType::Dint => "Integer32",
        FieldType::Real => "Float32",
    }
}

/// Render the GSDML document for `cfg`.
///
/// Deviations from a plain textbook template, both made to match
/// `GSDML-V2.4-RT-Labs-P-Net-Sample-App-20220324.xml` (the reference file TIA
/// accepted), rather than inventing schema usage:
/// - `DeviceAccessPointItem` carries `PNIO_Version="V2.4"` (present on the reference's
///   DAP; declares the supported PROFINET version — needed together with
///   `StartupMode="Advanced"`).
/// - `PortSubmoduleItem` lists its MAU type via a `<MAUTypeList><MAUTypeItem .../>`
///   child, as the reference does, instead of a flat `MAUTypes` attribute (which does
///   not appear anywhere in the reference). No `<SubslotList>` element is emitted
///   under `DeviceAccessPointItem` either: the reference has none, and the interface
///   and port subslot numbers are already carried as `SubslotNumber` attributes on
///   `InterfaceSubmoduleItem`/`PortSubmoduleItem` below.
pub fn render(cfg: &DeviceConfig, meta: &GsdmlMeta) -> String {
    let mut x = String::with_capacity(16 * 1024);
    let mut texts: Vec<(String, String)> = Vec::new(); // (TextId, Value)
    let n_slots = cfg.submodules().last().map(|s| s.slot.0).unwrap_or(0);
    let (max_in, max_out) = cfg.submodules().iter().fold((0u32, 0u32), |(i, o), s| {
        (
            i + cfg.input_len(s.slot).unwrap_or(0) as u32,
            o + cfg.output_len(s.slot).unwrap_or(0) as u32,
        )
    });
    let send_clock = if cfg.min_device_interval() < 32 {
        format!("{} 32", cfg.min_device_interval())
    } else {
        "32".to_string()
    };
    let _ = write!(
        x,
        r#"<?xml version="1.0" encoding="utf-8"?>
<ISO15745Profile xmlns="http://www.profibus.com/GSDML/2003/11/DeviceProfile" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.profibus.com/GSDML/2003/11/DeviceProfile ..\xsd\GSDML-DeviceProfile-V2.4.xsd">
  <ProfileHeader>
    <ProfileIdentification>PROFINET Device Profile</ProfileIdentification>
    <ProfileRevision>1.00</ProfileRevision>
    <ProfileName>Device Profile for PROFINET Devices</ProfileName>
    <ProfileSource>PROFIBUS Nutzerorganisation e. V. (PNO)</ProfileSource>
    <ProfileClassID>Device</ProfileClassID>
    <ISO15745Reference>
      <ISO15745Part>4</ISO15745Part>
      <ISO15745Edition>1</ISO15745Edition>
      <ProfileTechnology>GSDML</ProfileTechnology>
    </ISO15745Reference>
  </ProfileHeader>
  <ProfileBody>
    <DeviceIdentity VendorID="0x{vid:04X}" DeviceID="0x{did:04X}">
      <InfoText TextId="T_InfoText"/>
      <VendorName Value="{vendor}"/>
    </DeviceIdentity>
    <DeviceFunction>
      <Family MainFamily="I/O" ProductFamily="{family}"/>
    </DeviceFunction>
    <ApplicationProcess>
      <DeviceAccessPointList>
        <DeviceAccessPointItem ID="DAP1" PNIO_Version="V2.4" PhysicalSlots="0..{n_slots}" ModuleIdentNumber="0x00000001" MinDeviceInterval="{mdi}" DNS_CompatibleName="{station}" FixedInSlots="0" ObjectUUID_LocalIndex="1" MultipleWriteSupported="true" DeviceAccessSupported="false" CheckDeviceID_Allowed="true" NameOfStationNotTransferable="false">
          <ModuleInfo>
            <Name TextId="T_DAP_Name"/>
            <InfoText TextId="T_DAP_Info"/>
            <VendorName Value="{vendor}"/>
            <OrderNumber Value="{order}"/>
            <HardwareRelease Value="1.0"/>
            <SoftwareRelease Value="V0.0.0"/>
          </ModuleInfo>
          <IOConfigData MaxInputLength="{max_in}" MaxOutputLength="{max_out}"/>
          <UseableModules>
"#,
        vid = cfg.vendor_id(),
        did = cfg.device_id(),
        vendor = escape(&meta.vendor_name),
        family = escape(&meta.product_family),
        n_slots = n_slots,
        mdi = cfg.min_device_interval(),
        station = escape(cfg.station_name()),
        order = escape(&meta.order_number),
        max_in = max_in,
        max_out = max_out
    );
    for s in cfg.submodules() {
        let _ = writeln!(
            x,
            r#"            <ModuleItemRef ModuleItemTarget="M{n}" AllowedInSlots="{n}"/>"#,
            n = s.slot.0
        );
    }
    let _ = write!(
        x,
        r#"          </UseableModules>
          <VirtualSubmoduleList>
            <VirtualSubmoduleItem ID="DAP1_SM" SubmoduleIdentNumber="0x00000001" MayIssueProcessAlarm="false">
              <IOData/>
              <ModuleInfo>
                <Name TextId="T_DAP_Name"/>
                <InfoText TextId="T_DAP_Info"/>
              </ModuleInfo>
            </VirtualSubmoduleItem>
          </VirtualSubmoduleList>
          <SystemDefinedSubmoduleList>
            <InterfaceSubmoduleItem ID="DAP1_IF" SubslotNumber="32768" TextId="T_Interface" SubmoduleIdentNumber="0x00008000" SupportedRT_Classes="RT_CLASS_1" SupportedProtocols="SNMP;LLDP" DCP_HelloSupported="false" PTP_BoundarySupported="false" DCP_BoundarySupported="false" DelayMeasurementSupported="false">
              <ApplicationRelations StartupMode="Advanced">
                <TimingProperties SendClock="{send_clock}" ReductionRatio="1 2 4 8 16 32 64 128 256 512"/>
              </ApplicationRelations>
            </InterfaceSubmoduleItem>
            <PortSubmoduleItem ID="DAP1_P1" SubslotNumber="32769" TextId="T_Port1" SubmoduleIdentNumber="0x00008001" MaxPortTxDelay="160" MaxPortRxDelay="350">
              <MAUTypeList>
                <MAUTypeItem Value="16"/>
              </MAUTypeList>
            </PortSubmoduleItem>
          </SystemDefinedSubmoduleList>
        </DeviceAccessPointItem>
      </DeviceAccessPointList>
      <ModuleList>
"#,
        send_clock = send_clock
    );
    texts.push(("T_InfoText".into(), escape(&meta.info_text)));
    texts.push((
        "T_DAP_Name".into(),
        format!("{} DAP", escape(&meta.product_family)),
    ));
    texts.push(("T_DAP_Info".into(), "Device access point".into()));
    texts.push(("T_Interface".into(), "PROFINET interface".into()));
    texts.push(("T_Port1".into(), "Port 1".into()));

    for s in cfg.submodules() {
        let n = s.slot.0;
        let _ = write!(
            x,
            r#"        <ModuleItem ID="M{n}" ModuleIdentNumber="0x{ident:08X}">
          <ModuleInfo>
            <Name TextId="M{n}_Name"/>
            <InfoText TextId="M{n}_Info"/>
            <OrderNumber Value="{order}-M{n}"/>
          </ModuleInfo>
          <VirtualSubmoduleList>
            <VirtualSubmoduleItem ID="M{n}_SM" SubmoduleIdentNumber="0x00000001" MayIssueProcessAlarm="false">
              <IOData>
"#,
            ident = 0x100u32 + n as u32,
            order = escape(&meta.order_number)
        );
        texts.push((format!("M{n}_Name"), escape(&s.name)));
        texts.push((
            format!("M{n}_Info"),
            format!(
                "Slot {n}: {} input bytes, {} output bytes",
                cfg.input_len(s.slot).unwrap_or(0),
                cfg.output_len(s.slot).unwrap_or(0)
            ),
        ));
        for (dir, tag, prefix) in [
            (Direction::Input, "Input", "In"),
            (Direction::Output, "Output", "Out"),
        ] {
            let Some(fields) = cfg.fields(s.slot, dir) else {
                continue;
            };
            let _ = writeln!(
                x,
                r#"                <{tag} Consistency="All items consistency">"#
            );
            for item in items(fields) {
                match item {
                    Item::Scalar(ty, i) => {
                        let id = format!("M{n}_{prefix}{i}");
                        let _ = writeln!(
                            x,
                            r#"                  <DataItem DataType="{}" TextId="{id}"/>"#,
                            data_type(ty)
                        );
                        texts.push((id, format!("{prefix} {i} ({ty:?})")));
                    }
                    Item::Bits(group) => {
                        let first = group[0].0;
                        let id = format!("M{n}_{prefix}{first}_bits");
                        let _ = writeln!(
                            x,
                            r#"                  <DataItem DataType="Unsigned8" UseAsBits="true" TextId="{id}">"#
                        );
                        texts.push((
                            id,
                            format!("{prefix} bits {}..{}", first, group.last().unwrap().0),
                        ));
                        for (i, f) in &group {
                            let bid = format!("M{n}_{prefix}{i}_b");
                            let _ = writeln!(
                                x,
                                r#"                    <BitDataItem BitOffset="{}" TextId="{bid}"/>"#,
                                f.bit
                            );
                            texts.push((bid, format!("{prefix} {i} (Bool)")));
                        }
                        let _ = writeln!(x, "                  </DataItem>");
                    }
                }
            }
            let _ = writeln!(x, "                </{tag}>");
        }
        let _ = write!(
            x,
            r#"              </IOData>
              <ModuleInfo>
                <Name TextId="M{n}_Name"/>
                <InfoText TextId="M{n}_Info"/>
              </ModuleInfo>
            </VirtualSubmoduleItem>
          </VirtualSubmoduleList>
        </ModuleItem>
"#
        );
    }
    let _ = write!(
        x,
        r#"      </ModuleList>
      <ExternalTextList>
        <PrimaryLanguage>
"#
    );
    for (id, value) in &texts {
        let _ = writeln!(x, r#"          <Text TextId="{id}" Value="{value}"/>"#);
    }
    let _ = write!(
        x,
        r#"        </PrimaryLanguage>
      </ExternalTextList>
    </ApplicationProcess>
  </ProfileBody>
</ISO15745Profile>
"#
    );
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeviceConfig, Slot};
    use crate::data::FieldType::*;

    fn sample() -> DeviceConfig {
        DeviceConfig::builder("pnio-dev")
            .input(Slot(1), &[Real; 16])
            .input(Slot(2), &[Bool; 32])
            .output(Slot(3), &[Real; 16])
            .output(Slot(4), &[Bool; 32])
            .build()
            .unwrap()
    }

    fn meta() -> GsdmlMeta {
        GsdmlMeta {
            vendor_name: "Core Engineering".into(),
            product_family: "pnio".into(),
            info_text: "pnio sample device: 16 REAL + 32 BOOL per direction".into(),
            order_number: "PNIO-SAMPLE".into(),
            date: (2026, 8, 29),
        }
    }

    #[test]
    fn file_name_follows_the_tia_pattern() {
        assert_eq!(
            file_name(&meta()),
            "GSDML-V2.4-CoreEngineering-pnio-20260829.xml"
        );
    }

    #[test]
    fn escape_handles_the_five_xml_specials() {
        assert_eq!(escape("a<b&c>\"d'"), "a&lt;b&amp;c&gt;&quot;d&apos;");
    }

    #[test]
    fn render_matches_the_golden() {
        let got = render(&sample(), &meta());
        let want = std::fs::read_to_string(format!(
            "{}/testdata/gsdml/sample-16real-32bool.xml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        assert_eq!(got, want.replace("\r\n", "\n"));
    }

    #[test]
    fn render_is_well_formed_and_structurally_consistent() {
        let cfg = sample();
        let xml = render(&cfg, &meta());
        let doc = roxmltree::Document::parse(&xml).expect("well-formed");
        let find = |name: &str| {
            doc.descendants()
                .filter(move |n| n.has_tag_name(name))
                .collect::<Vec<_>>()
        };
        let ident = find("DeviceIdentity")[0];
        assert_eq!(ident.attribute("VendorID"), Some("0xFFFF"));
        assert_eq!(ident.attribute("DeviceID"), Some("0x0001"));
        let dap = find("DeviceAccessPointItem")[0];
        assert_eq!(dap.attribute("DNS_CompatibleName"), Some("pnio-dev"));
        assert_eq!(dap.attribute("MinDeviceInterval"), Some("32"));
        assert_eq!(dap.attribute("PhysicalSlots"), Some("0..4"));
        // Required by the PI XSD V2.4 on DeviceAccessPointItem (values as in the
        // rt-labs reference file TIA accepted).
        assert_eq!(dap.attribute("CheckDeviceID_Allowed"), Some("true"));
        assert_eq!(dap.attribute("NameOfStationNotTransferable"), Some("false"));
        for tag in ["Input", "Output"] {
            for n in find(tag) {
                assert_eq!(
                    n.attribute("Consistency"),
                    Some("All items consistency"),
                    "{tag}"
                );
            }
        }
        let modules = find("ModuleItem");
        assert_eq!(modules.len(), 4);
        let mac = crate::eth::MacAddr([0; 6]);
        let model = cfg.model(mac);
        for (m, s) in modules.iter().zip(&model.slots[1..]) {
            assert_eq!(
                m.attribute("ModuleIdentNumber"),
                Some(format!("0x{:08X}", s.module_ident).as_str())
            );
        }
        let refs = find("ModuleItemRef");
        let allowed: Vec<_> = refs
            .iter()
            .map(|r| r.attribute("AllowedInSlots").unwrap())
            .collect();
        assert_eq!(allowed, vec!["1", "2", "3", "4"]);
        // 16 REAL -> 16 Float32 DataItems; 32 BOOL -> 4 Unsigned8 UseAsBits with 8 BitDataItems each
        let items = find("DataItem");
        assert_eq!(
            items
                .iter()
                .filter(|i| i.attribute("DataType") == Some("Float32"))
                .count(),
            32
        );
        let bit_items: Vec<_> = items
            .iter()
            .filter(|i| i.attribute("UseAsBits") == Some("true"))
            .collect();
        assert_eq!(bit_items.len(), 8);
        for b in &bit_items {
            let offsets: Vec<_> = b
                .children()
                .filter(|c| c.has_tag_name("BitDataItem"))
                .map(|c| c.attribute("BitOffset").unwrap().to_string())
                .collect();
            assert_eq!(offsets, ["0", "1", "2", "3", "4", "5", "6", "7"]);
        }
        // every TextId is defined
        let defined: std::collections::HashSet<_> = find("Text")
            .iter()
            .map(|t| t.attribute("TextId").unwrap().to_string())
            .collect();
        for n in doc.descendants() {
            if let Some(id) = n.attribute("TextId") {
                if !n.has_tag_name("Text") {
                    assert!(defined.contains(id), "TextId {id} undefined");
                }
            }
        }
        let timing = find("TimingProperties")[0];
        assert_eq!(timing.attribute("SendClock"), Some("32"));
    }

    #[test]
    fn interval_16_declares_both_send_clocks() {
        let cfg = DeviceConfig::builder("a")
            .min_device_interval(16)
            .input(Slot(1), &[Int])
            .build()
            .unwrap();
        let xml = render(&cfg, &meta());
        assert!(xml.contains("MinDeviceInterval=\"16\""));
        assert!(xml.contains("SendClock=\"16 32\""));
    }

    #[test]
    fn mixed_submodule_renders_input_and_output_lists_and_a_partial_bit_group() {
        let cfg = DeviceConfig::builder("a")
            .submodule(Slot(5), "mixed", &[Int, Bool, Bool, Bool], &[Dint])
            .build()
            .unwrap();
        let xml = render(&cfg, &meta());
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let input = doc.descendants().find(|n| n.has_tag_name("Input")).unwrap();
        let items: Vec<_> = input
            .children()
            .filter(|c| c.has_tag_name("DataItem"))
            .collect();
        assert_eq!(items[0].attribute("DataType"), Some("Integer16"));
        assert_eq!(items[1].attribute("DataType"), Some("Unsigned8"));
        assert_eq!(
            items[1]
                .children()
                .filter(|c| c.has_tag_name("BitDataItem"))
                .count(),
            3
        );
        let output = doc
            .descendants()
            .find(|n| n.has_tag_name("Output"))
            .unwrap();
        assert_eq!(
            output
                .children()
                .find(|c| c.has_tag_name("DataItem"))
                .unwrap()
                .attribute("DataType"),
            Some("Integer32")
        );
    }
}
