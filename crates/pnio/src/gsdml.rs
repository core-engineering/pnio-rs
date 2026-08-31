//! GSDML V2.4 rendering from a [`DeviceConfig`]: same object, same idents, same field
//! order as the code, so the file cannot drift from what the device answers (spec §5).
//! Text template; no XML library in the crate.

use crate::config::{DeviceConfig, Direction, FieldRef};
use crate::data::FieldType;
use std::fmt::Write;

/// Vendor/product texts and the date that go into the file (and its name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsdmlMeta {
    /// `VendorName`/`DeviceIdentity` text; also cleaned to `[A-Za-z0-9]` for the file
    /// name — see [`file_name`].
    pub vendor_name: String,
    /// `ProductFamily` text; also cleaned to `[A-Za-z0-9]` for the file name — see
    /// [`file_name`].
    pub product_family: String,
    /// `InfoText` shown for the device in TIA's hardware catalog.
    pub info_text: String,
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

/// The three `ModuleInfo` identity elements every module carries — the DAP and each
/// `ModuleItem` alike: the very same `OrderID`, hardware revision and software
/// revision the device answers in its I&M0 record, so the GSDML TIA reads and the
/// wire identity it later checks can never drift (Plan 6 rule, spec §4.4).
///
/// Rendered at `ModuleInfo`'s child indentation, without a trailing newline.
fn module_identity_xml(cfg: &DeviceConfig) -> String {
    let im0 = cfg.im0();
    format!(
        concat!(
            r#"            <OrderNumber Value="{order}"/>"#,
            "\n",
            r#"            <HardwareRelease Value="{hw}"/>"#,
            "\n",
            r#"            <SoftwareRelease Value="{prefix}{func}.{bug}.{internal}"/>"#,
        ),
        order = escape(&im0.order_id),
        hw = im0.hardware_revision,
        prefix = im0.software_revision.prefix,
        func = im0.software_revision.functional,
        bug = im0.software_revision.bug_fix,
        internal = im0.software_revision.internal,
    )
}

/// Render the GSDML document for `cfg`.
///
/// Deviations from a plain textbook template, both made to match
/// `GSDML-V2.4-RT-Labs-P-Net-Sample-App-20220324.xml` (the reference file TIA
/// accepted), rather than inventing schema usage:
/// - `DeviceAccessPointItem` carries `PNIO_Version="V2.3"`, not the reference's `"V2.4"`:
///   TIA's GSD checker applies version-dependent checks, and for `PNIO_Version >=
///   "V2.31"` it mandates `CertificationInfo` and `LLDP_NoD_Supported="true"` on the DAP,
///   `ResetToFactoryModes="2"`, and `PTP_BoundarySupported="true"`/
///   `DCP_BoundarySupported="true"` on `InterfaceSubmoduleItem` — none of which the device
///   implements (no LLDP, no PTP/DCP boundary, no ResetToFactory). V2.3 is the last
///   profile version without those mandates and still allows `StartupMode="Advanced"`.
///   Revisit once LLDP/boundary/ResetToFactory support lands (Plan 5+).
/// - `InterfaceSubmoduleItem` carries `SupportedProtocols=""` rather than the
///   reference's `"SNMP;LLDP"`: the device implements neither. The v2.4 XSD makes the
///   attribute `use="required"`, but its type (`base:TokenListT`, pattern
///   `(([0-9a-zA-Z_]+;)*[0-9a-zA-Z_]+)?`) allows an empty token list, so an empty value
///   is the honest declaration — omitting the attribute fails XSD validation.
/// - `PortSubmoduleItem` lists its MAU type via a `<MAUTypeList><MAUTypeItem .../>`
///   child, as the reference does, instead of a flat `MAUTypes` attribute (which does
///   not appear anywhere in the reference). No `<SubslotList>` element is emitted
///   under `DeviceAccessPointItem` either: the reference has none, and the interface
///   and port subslot numbers are already carried as `SubslotNumber` attributes on
///   `InterfaceSubmoduleItem`/`PortSubmoduleItem` below.
/// - `IOConfigData`'s `MaxInputLength`/`MaxOutputLength` are the *CR* C-SDU lengths
///   ([`DeviceConfig::input_cr_len`]/[`DeviceConfig::output_cr_len`]), not the sum of
///   the submodules' data lengths: TIA counts the IOPS byte that closes each
///   submodule with data in that direction plus the IOCS byte reserved for each
///   submodule with data only in the other direction (and the 3-byte DAP IOPS/IOCS),
///   the same accounting `config::check_total_csdu` guards against the 1440-byte RT
///   frame budget. The reference file declares flat data sums (`"244"`/`"244"`); this
///   crate declares the exact CR sizes instead, matching what TIA itself computes.
///   `MaxDataLength` (present in the V2.4 XSD, optional) is their sum.
pub fn render(cfg: &DeviceConfig, meta: &GsdmlMeta) -> String {
    let mut x = String::with_capacity(16 * 1024);
    let mut texts: Vec<(String, String)> = Vec::new(); // (TextId, Value)
    let n_slots = cfg.submodules().last().map(|s| s.slot.0).unwrap_or(0);
    let max_in = cfg.input_cr_len();
    let max_out = cfg.output_cr_len();
    let max_data = max_in as u32 + max_out as u32;
    let send_clock = if cfg.min_device_interval() < 32 {
        format!("{} 32", cfg.min_device_interval())
    } else {
        "32".to_string()
    };
    let identity = module_identity_xml(cfg);
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
        <DeviceAccessPointItem ID="DAP1" PNIO_Version="V2.3" PhysicalSlots="0..{n_slots}" ModuleIdentNumber="0x00000001" MinDeviceInterval="{mdi}" DNS_CompatibleName="{station}" FixedInSlots="0" ObjectUUID_LocalIndex="1" MultipleWriteSupported="true" DeviceAccessSupported="false" CheckDeviceID_Allowed="true" NameOfStationNotTransferable="false">
          <ModuleInfo>
            <Name TextId="T_DAP_Name"/>
            <InfoText TextId="T_DAP_Info"/>
            <VendorName Value="{vendor}"/>
{identity}
          </ModuleInfo>
          <IOConfigData MaxInputLength="{max_in}" MaxOutputLength="{max_out}" MaxDataLength="{max_data}"/>
          <UseableModules>
"#,
        vid = cfg.vendor_id(),
        did = cfg.device_id(),
        vendor = escape(&meta.vendor_name),
        family = escape(&meta.product_family),
        n_slots = n_slots,
        mdi = cfg.min_device_interval(),
        station = escape(cfg.station_name()),
        identity = identity,
        max_in = max_in,
        max_out = max_out,
        max_data = max_data
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
            <VirtualSubmoduleItem ID="DAP1_SM" SubmoduleIdentNumber="0x00000001" MayIssueProcessAlarm="false" Writeable_IM_Records="1 2 3">
              <IOData/>
              <ModuleInfo>
                <Name TextId="T_DAP_Name"/>
                <InfoText TextId="T_DAP_Info"/>
              </ModuleInfo>
            </VirtualSubmoduleItem>
          </VirtualSubmoduleList>
          <SystemDefinedSubmoduleList>
            <InterfaceSubmoduleItem ID="DAP1_IF" SubslotNumber="32768" TextId="T_Interface" SubmoduleIdentNumber="0x00008000" SupportedRT_Classes="RT_CLASS_1" SupportedProtocols="" DCP_HelloSupported="false" PTP_BoundarySupported="false" DCP_BoundarySupported="false" DelayMeasurementSupported="false">
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
{identity}
          </ModuleInfo>
          <VirtualSubmoduleList>
            <VirtualSubmoduleItem ID="M{n}_SM" SubmoduleIdentNumber="0x00000001" MayIssueProcessAlarm="false">
              <IOData>
"#,
            ident = 0x100u32 + n as u32,
            identity = identity
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
            .im0(crate::im::Im0 {
                order_id: "PNIO-SAMPLE".into(),
                ..crate::im::Im0::default()
            })
            .build()
            .unwrap()
    }

    fn meta() -> GsdmlMeta {
        GsdmlMeta {
            vendor_name: "Core Engineering".into(),
            product_family: "pnio".into(),
            info_text: "pnio sample device: 16 REAL + 32 BOOL per direction".into(),
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
        // Input/Output CR C-SDU lengths including the IOPS/IOCS bytes (the same
        // accounting `config::check_total_csdu` guards), not the plain data sums
        // (64 + 4 = 68): 3 (DAP IOPS) + (64+1) [slot1 in] + (4+1) [slot2 in] +
        // 1 [slot3 out-only IOCS] + 1 [slot4 out-only IOCS] = 75, symmetrically for
        // output.
        let ioconfig = find("IOConfigData")[0];
        assert_eq!(ioconfig.attribute("MaxInputLength"), Some("75"));
        assert_eq!(ioconfig.attribute("MaxOutputLength"), Some("75"));
        assert_eq!(ioconfig.attribute("MaxDataLength"), Some("150"));
        // Required by the PI XSD V2.4 on DeviceAccessPointItem (values as in the
        // rt-labs reference file TIA accepted).
        assert_eq!(dap.attribute("CheckDeviceID_Allowed"), Some("true"));
        assert_eq!(dap.attribute("NameOfStationNotTransferable"), Some("false"));
        // V2.3, not V2.4: TIA's GSD checker mandates LLDP/PTP-DCP boundary/ResetToFactory/
        // CertificationInfo claims from PNIO_Version >= "V2.31", none of which the device
        // implements (see the module doc).
        assert_eq!(dap.attribute("PNIO_Version"), Some("V2.3"));
        let iface = find("InterfaceSubmoduleItem")[0];
        // The v2.4 XSD makes SupportedProtocols required; an empty value is the honest
        // declaration since the device implements neither SNMP nor LLDP.
        assert_eq!(iface.attribute("SupportedProtocols"), Some(""));
        for id in [
            "LLDP_NoD_Supported",
            "ResetToFactoryModes",
            "CertificationInfo",
        ] {
            assert!(
                !xml.contains(id),
                "unimplemented feature claim {id} must not appear in the document"
            );
        }
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
        let dap_sm = find("VirtualSubmoduleItem")[0];
        assert_eq!(dap_sm.attribute("ID"), Some("DAP1_SM"));
        assert_eq!(dap_sm.attribute("Writeable_IM_Records"), Some("1 2 3"));
        // Every ModuleInfo — the DAP's and each module's — carries the *same* wire
        // identity as the I&M0 record: no per-module OrderNumber suffix, so what TIA
        // reads from the GSDML is what the device answers on 0xAFF0.
        let infos: Vec<_> = find("ModuleInfo")
            .into_iter()
            .filter(|n| {
                n.parent().is_some_and(|p| {
                    p.has_tag_name("DeviceAccessPointItem") || p.has_tag_name("ModuleItem")
                })
            })
            .collect();
        assert_eq!(
            infos.len(),
            5,
            "1 DAP + 4 modules (a VirtualSubmoduleItem's own ModuleInfo carries names only)"
        );
        for info in &infos {
            let value = |tag: &str| {
                info.children()
                    .find(|c| c.has_tag_name(tag))
                    .unwrap_or_else(|| panic!("{tag} missing from a ModuleInfo"))
                    .attribute("Value")
                    .map(str::to_string)
            };
            assert_eq!(value("OrderNumber"), Some(cfg.im0().order_id.clone()));
            assert_eq!(
                value("HardwareRelease"),
                Some(cfg.im0().hardware_revision.to_string())
            );
            assert_eq!(value("SoftwareRelease"), Some("V0.1.0".to_string()));
        }
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
