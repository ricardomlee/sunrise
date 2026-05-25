use sunrise_config::SunriseConfig;

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub hostname: String,
    pub unique_id: String,
    pub uuid: String,
    pub local_ip: String,
    pub external_ip: String,
    pub pair_status: u8,
    pub current_game: u32,
    pub state: String,
    pub app_version: String,
    pub gfe_version: String,
    pub server_codec_mode_support: u32,
    pub supported_display_mode: String,
    pub mac: String,
    pub https_port: u16,
    pub http_port: u16,
    pub rtsp_port: u16,
}

impl ServerInfo {
    pub fn from_config(config: &SunriseConfig, local_ip: impl Into<String>, paired: bool) -> Self {
        let local_ip = local_ip.into();
        Self {
            hostname: config.host_name.clone(),
            unique_id: config.unique_id.clone(),
            uuid: config.uuid.clone(),
            local_ip: local_ip.clone(),
            external_ip: local_ip,
            pair_status: u8::from(paired),
            current_game: 0,
            state: "ONLINE".to_string(),
            app_version: "7.1.431.-1".to_string(),
            gfe_version: "3.23.0.74".to_string(),
            server_codec_mode_support: 257,
            supported_display_mode: "1920x1080x60".to_string(),
            mac: config.mac_address.clone(),
            https_port: config.https_port,
            http_port: config.http_port,
            rtsp_port: config.rtsp_port,
        }
    }
}

pub fn serverinfo_xml(info: &ServerInfo) -> String {
    let fields = [
        ("hostname", info.hostname.as_str()),
        ("uniqueid", info.unique_id.as_str()),
        ("uuid", info.uuid.as_str()),
        ("LocalIP", info.local_ip.as_str()),
        ("ExternalIP", info.external_ip.as_str()),
        ("PairStatus", &info.pair_status.to_string()),
        ("currentgame", &info.current_game.to_string()),
        ("state", info.state.as_str()),
        ("appversion", info.app_version.as_str()),
        ("GfeVersion", info.gfe_version.as_str()),
        (
            "serverCodecModeSupport",
            &info.server_codec_mode_support.to_string(),
        ),
        ("supportedDisplayMode", info.supported_display_mode.as_str()),
        ("mac", info.mac.as_str()),
        ("HTTPSPort", &info.https_port.to_string()),
        ("HTTPPort", &info.http_port.to_string()),
        ("RTSPPort", &info.rtsp_port.to_string()),
    ];

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<root status_code=\"200\">\n");
    for (name, value) in fields {
        xml.push_str("  <");
        xml.push_str(name);
        xml.push('>');
        xml.push_str(&escape_xml(value));
        xml.push_str("</");
        xml.push_str(name);
        xml.push_str(">\n");
    }
    xml.push_str("</root>\n");
    xml
}

pub fn applist_xml() -> String {
    [
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<root protocol_version=\"0.1\" query=\"applist\" status_code=\"200\" status_message=\"OK\">",
        "  <App>",
        "    <AppInstallPath>C:\\\\Windows\\\\explorer.exe</AppInstallPath>",
        "    <ID>1</ID>",
        "    <AppTitle>Desktop</AppTitle>",
        "    <CmsId>0</CmsId>",
        "    <Distributor>Standalone</Distributor>",
        "    <IsAppCollectorGame>0</IsAppCollectorGame>",
        "    <IsHdrSupported>0</IsHdrSupported>",
        "    <MaxControllersForSingleSession>1</MaxControllersForSingleSession>",
        "    <ShortName>desktop</ShortName>",
        "    <SupportedSOPS>",
        "      <SOPS>",
        "        <Height>1080</Height>",
        "        <RefreshRate>60</RefreshRate>",
        "        <Width>1920</Width>",
        "      </SOPS>",
        "    </SupportedSOPS>",
        "    <UniqueId>1</UniqueId>",
        "    <simulateControllers>0</simulateControllers>",
        "  </App>",
        "</root>",
        "",
    ]
    .join("\n")
}

pub fn pair_xml<'a>(fields: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<root status_code=\"200\">\n");
    for (name, value) in fields {
        xml.push_str("  <");
        xml.push_str(name);
        xml.push('>');
        xml.push_str(&escape_xml(value));
        xml.push_str("</");
        xml.push_str(name);
        xml.push_str(">\n");
    }
    xml.push_str("</root>\n");
    xml
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SunriseConfig {
        SunriseConfig {
            host_name: "test-host".to_string(),
            http_port: 47989,
            https_port: 47984,
            rtsp_port: 48010,
            unique_id: "ABCDEF0123456789".to_string(),
            uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            mac_address: "02:AA:BB:CC:DD:EE".to_string(),
            server_cert_pem: None,
            server_private_key_pem: None,
            paired_clients: Vec::new(),
        }
    }

    #[test]
    fn serverinfo_xml_contains_required_fields() {
        let info = ServerInfo::from_config(&test_config(), "192.0.2.10", false);
        let xml = serverinfo_xml(&info);

        for field in [
            "hostname",
            "uniqueid",
            "uuid",
            "LocalIP",
            "ExternalIP",
            "PairStatus",
            "currentgame",
            "state",
            "appversion",
            "GfeVersion",
            "serverCodecModeSupport",
            "supportedDisplayMode",
            "mac",
            "HTTPSPort",
            "HTTPPort",
            "RTSPPort",
        ] {
            assert!(xml.contains(&format!("<{field}>")), "missing {field}");
            assert!(xml.contains(&format!("</{field}>")), "missing {field}");
        }
    }

    #[test]
    fn serverinfo_uses_stable_ids_from_config() {
        let info = ServerInfo::from_config(&test_config(), "192.0.2.10", false);
        let xml = serverinfo_xml(&info);

        assert!(xml.contains("<uniqueid>ABCDEF0123456789</uniqueid>"));
        assert!(xml.contains("<uuid>550e8400-e29b-41d4-a716-446655440000</uuid>"));
    }

    #[test]
    fn serverinfo_advertises_gen_7_pairing_version() {
        let info = ServerInfo::from_config(&test_config(), "192.0.2.10", false);
        let xml = serverinfo_xml(&info);

        assert!(xml.contains("<appversion>7.1.431.-1</appversion>"));
    }

    #[test]
    fn applist_contains_desktop_app() {
        let xml = applist_xml();

        assert!(xml.contains("<ID>1</ID>"));
        assert!(xml.contains("<AppTitle>Desktop</AppTitle>"));
        assert!(xml.contains("<IsHdrSupported>0</IsHdrSupported>"));
        assert!(xml.contains("<ShortName>desktop</ShortName>"));
        assert!(xml.contains("<SupportedSOPS>"));
    }

    #[test]
    fn pair_xml_contains_supplied_fields() {
        let xml = pair_xml([("paired", "1"), ("plaincert", "ABCD")]);

        assert!(xml.contains("<paired>1</paired>"));
        assert!(xml.contains("<plaincert>ABCD</plaincert>"));
    }
}
