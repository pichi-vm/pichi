// SPDX-License-Identifier: Apache-2.0

//! Independent known-answer vectors for the algorithm selection and salt order.

#![cfg(any(
    feature = "sha1",
    feature = "sha2",
    feature = "sha3",
    feature = "ripemd",
    feature = "whirlpool",
    feature = "streebog",
    feature = "sm3",
    feature = "blake2"
))]

use std::{fmt::Write as _, io::Cursor, num::NonZero};

use devmap_core::{BlockSize, Geometry};
use devmap_verity::{Algorithm, Format as _, HashType, Header, Parameters, Salt};

// Each vector hashes 512 bytes of 0x5a, with salt 010203 prepended
// (Normal) or appended (ChromeOs). Generated independently with Python
// hashlib; Whirlpool and Streebog use libgcrypt's gcry_md_hash_buffer.
// BLAKE2 uses its output-length parameter, not truncated full digests.
const VECTORS: &[(Algorithm, &str, &str)] = &[
    #[cfg(feature = "sha1")]
    (
        Algorithm::Sha1,
        "76f86b1a6b2fcd5b96f2a21502a5316cafd43473",
        "00aaf98264dba54049374f71ff11cf286fdb90a0",
    ),
    #[cfg(feature = "sha2")]
    (
        Algorithm::Sha224,
        "f2b0777d44ae789c1ca7d225da0876cbad9248af412f7aae49ca7229",
        "c85b7b9b6d4eb3bd999f62e3c1c9587a559320d1bd2aa7cf74604975",
    ),
    #[cfg(feature = "sha2")]
    (
        Algorithm::Sha256,
        "e704a0e52370efaf2af0fa11453cce0c752c6929bbfe81a90e1b1c7870db7946",
        "2981a9f6de7aeb30b05f4f22031fd2f74e8b61a26d180d029357f764811ce096",
    ),
    #[cfg(feature = "sha2")]
    (
        Algorithm::Sha384,
        "0944380b66534b5fd928983a17726244705bded5f73ad343c1b41eb17e93e28543ec5f993e4833d6d0a9bf3c874fc36d",
        "86cad98a1556e4f48e2ea0ff61889050bd79faf24d2b2b3056eda70979e71addca4decb8b4798ba317da36d37e359cd8",
    ),
    #[cfg(feature = "sha2")]
    (
        Algorithm::Sha512,
        "40ccc68dc28e98838c07b7f14946b7412e731167d763b889438834c95a4fff69342c4551f667e986a3a120c7f1c42c87ba2d6c02015e44ebcb829463a62224e6",
        "b4717dfb3f4cf23982ecae14049637d67e092c8342cc4183abce0ecede2931d49c47c109e017b4ca80cb9d084c99e09f1a886f11b34894a4384a765465368531",
    ),
    #[cfg(feature = "ripemd")]
    (
        Algorithm::Ripemd160,
        "cf4c0c8643c100155fbf6ed563008300d28b47f3",
        "f042caf0291b79038b10e1cd8646e8b416f707a3",
    ),
    #[cfg(feature = "whirlpool")]
    (
        Algorithm::Whirlpool,
        "df1e9b9cf0ff27ea6c4bd9347af34c01bb799004c955a9d3859b65802adb00aecd4212e1361f16626dccd39ae2c8b3c6b1117838d9365064995f5b9e03bdccf4",
        "be5c7b73e4dd146cafbdad7cf76538141882f3d395f627b226a7463b83a627b7a023ad36956c6252a556cc55a67947d48bc5d479d0ac90e92159865eeb8c47a4",
    ),
    #[cfg(feature = "sha3")]
    (
        Algorithm::Sha3_224,
        "2237d900d54d0def884282feefc74354e7e2de8d84191ae24664b219",
        "3118dfa2bea09d806d13c5bfefdc6cfe3570e803c21b84e4e257b438",
    ),
    #[cfg(feature = "sha3")]
    (
        Algorithm::Sha3_256,
        "5b1e3fb92ca2621f848131c50560f7f10d7e4c616fe6ad6a8f1c43d5c83e566a",
        "dcb066a123d58d33a899e9c564c014d93da614083678242d25ddc29dae446821",
    ),
    #[cfg(feature = "sha3")]
    (
        Algorithm::Sha3_384,
        "224a524c3cca236a9cb5cc741ba68886aec5d83737ad94d4cef6262c2b43e98b031016cf14e113fe651166f33d3b0455",
        "29f3ad0a6c9a0d7e81bcba18c3ed172c6651e1876a8a2a980e7a4590bb98e480a9207a1b045c8929dde8faa60b55e119",
    ),
    #[cfg(feature = "sha3")]
    (
        Algorithm::Sha3_512,
        "2cbfe6c9d43ecc3c94c1b6cdb13d62254cbc858531bbbd3bda91ca08ec7ed4d6333402a205eba4db55426dcec51666dc4b8f69329c1e04038696d99a1327d14f",
        "ba441f8276452a299374943a32d0eceb6f61790508bf48a697aa5e53b761d697f8315e7ff99ab7b9c8811d4d3a774f96ed49c6898281088801997de316eccd14",
    ),
    #[cfg(feature = "streebog")]
    (
        Algorithm::Streebog256,
        "c983e528b1a7a1158555b9d4c3b1367f40ad05a1ef43bdfcb58161d8f0c889cb",
        "ad6389fac401deca7ec3045bfcfb50bf78a2b616e3841a0378cc777402895b9a",
    ),
    #[cfg(feature = "streebog")]
    (
        Algorithm::Streebog512,
        "3b16ea523c9869f49c04276858a5e0c842fc0eb8bf63365c0497e5517f3024423092a9a39737bede5df389912f838fa095bc74ee25722fac617462c888c78ff6",
        "1eca91e9e5e415d20469454a48ca5d57cf6efb13e47e6b4d3faaeb31597192e84d587a052b8a16bb65de538c7a93527cea6a9bedc65409bfeeddb253cf03fcfb",
    ),
    #[cfg(feature = "sm3")]
    (
        Algorithm::Sm3,
        "82747ff137ee132012e7a6daae86dd3e220a25cae61ee72a328dccf91c1343ef",
        "31e5805f8b240e6e969121cbc342cbd918758e5326a51bd6742f01a461fa63ed",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2b160,
        "eb29e9dd835770688989de093c4d4e21de09b3ae",
        "03fe5ca12b35acf2a0ac7ef90e93b7f73ff3d57d",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2b256,
        "37ac778b5ff54672e6519a81be673a6ecf461b9aced99051d8e3892b0fc5d8fa",
        "0b2d8278f7f252ca4460c90aa1d33079c7fe2ddb8c8772b9ddb2a1333ff325e2",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2b384,
        "393cc4e0baa7966e896e42734ea8ca77241e39b783e6b8f5abd1c5c8a47a38f3f18e3603244d4f97bc941d934115a798",
        "cad3df2266be6c98c0ea5f3aee4e58d3b7884ca7bbb973c2d8d03c9884df9d1dc8b2b802b5223ad384c9cdfde7bd0715",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2b512,
        "1cad95ad54813389adea1374f584974be9f5467e6090c79526a523c2c0863b84255bb599b59a69d7e2210911024a2ba92a1c947d9fc991e2773439bf63b70305",
        "649a4dea058eaa5b494a4738025cc1b16cfb3e2cd0c94a4f441cd6c757cdc8e8b5ef974cf07b08df4ae11d959b8e747482043882cd875f257eeadadd16c13c91",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2s128,
        "4a114d020fa2a6ca47c51257ec8f6847",
        "2e5bcc27b55ca9680acfeba08f6bbde8",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2s160,
        "075d8193a4d9ed80c6b81d6880a79f80bc706ee5",
        "c82fd964000ef7be53dba12b62f8078015a44011",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2s224,
        "c58c2ef8cf10ce4c2e012a8006c4c80a929a4d351dbc5bd8592ebd4e",
        "387166075078a1136dd52a0f1c4ee3fc1d02cb314d54626ec0d1ae96",
    ),
    #[cfg(feature = "blake2")]
    (
        Algorithm::Blake2s256,
        "70ed9b6ac4298c81dec2e5d0ed75d79d5d9ef012586607c04bc931fefb2e88d5",
        "5cc58527e07ece7e949ef7ebf2580c5404fe4e50d745f321c97ada3e1b6ed85b",
    ),
];
#[test]
fn salted_single_block_roots_match_independent_vectors() {
    for &(algorithm, normal, chrome_os) in VECTORS {
        for (hash_type, expected) in [(HashType::Normal, normal), (HashType::ChromeOs, chrome_os)] {
            let header = Header {
                uuid: [0; 16],
                parameters: Parameters {
                    hash_type,
                    algorithm,
                    salt: Salt::new(&[1, 2, 3]).unwrap(),
                    data: Geometry {
                        size: BlockSize::from_exponent(9).unwrap(),
                        count: NonZero::new(1).unwrap(),
                    },
                    hash: BlockSize::from_exponent(9).unwrap(),
                },
            };
            let mut output = Cursor::new(Vec::new());
            let root = header.format(&[0x5a; 512][..], &mut output).unwrap();
            let mut actual = String::new();
            for byte in root {
                write!(actual, "{byte:02x}").unwrap();
            }
            assert_eq!(actual, expected, "{algorithm:?} / {hash_type:?}");
            assert_eq!(output.into_inner().len(), 512);
        }
    }
}
