use serde::Serialize;

use crate::addr::AddrNet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    pub const ALL: [Network; 2] = [Network::Mainnet, Network::Testnet];

    pub fn name(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
        }
    }

    pub fn parse(s: &str) -> Option<Network> {
        Network::ALL.into_iter().find(|n| n.name() == s)
    }

    /// pchMessageStart from Dash Core's kernel/chainparams.cpp.
    pub fn magic(self) -> [u8; 4] {
        match self {
            Network::Mainnet => [0xbf, 0x0c, 0x6b, 0xbd],
            Network::Testnet => [0xce, 0xe2, 0xca, 0xff],
        }
    }

    /// Where MN_RR activated (`getdeploymentinfo`): from this block on, 37.5% of the masternode share
    /// of the block subsidy goes to Platform's credit pool (an OP_RETURN coinbase output), not the payee.
    pub fn mn_rr_height(self) -> i64 {
        match self {
            Network::Mainnet => 2_128_896,
            Network::Testnet => 1_066_900,
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Network::Mainnet => 9999,
            Network::Testnet => 19999,
        }
    }

    /// Default P2P port for addresses on `net`: I2P has no ports (SAM 3.1), so it's always 0.
    pub fn default_port_for(self, net: AddrNet) -> u16 {
        match net {
            AddrNet::I2p => 0,
            _ => self.default_port(),
        }
    }

    pub fn dns_seeds(self) -> &'static [&'static str] {
        match self {
            Network::Mainnet => &["dnsseed.dash.org"],
            Network::Testnet => &["testnet-seed.dashdot.io"],
        }
    }

    /// DIP0003EnforcementHeight: the first block whose masternode payee is a ProTx masternode.
    /// `masternode payments` has nothing to report below it.
    pub fn dip3_enforcement_height(self) -> u32 {
        match self {
            Network::Mainnet => 1_047_200,
            Network::Testnet => 7_300,
        }
    }
}
