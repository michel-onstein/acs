//! SHA-256 (FIPS 180-4), small enough to avoid a dependency. Used to check
//! that a remote install is byte-identical to what the client sent.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Incremental SHA-256.
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Sha256 {
            h: H0,
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self::default()
    }

    fn block(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in self.h.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *x = x.wrapping_add(y);
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let b = self.buf;
                self.block(&b);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let b: [u8; 64] = data[..64].try_into().unwrap();
            self.block(&b);
            data = &data[64..];
        }
        self.buf[..data.len()].copy_from_slice(data);
        self.buf_len += data.len();
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let fill = (64 + 56 - (self.buf_len + 1) % 64) % 64;
        pad.extend(std::iter::repeat(0u8).take(fill));
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        let mut out = [0u8; 32];
        for (i, v) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }
}

/// Digest of `data`.
pub fn digest(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finish()
}

pub fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_vectors() {
        assert_eq!(
            hex(&digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&digest(&million_a)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// Digests of the first `n` bytes of `(i * 7 + 1) % 256` for n in 0..=130,
    /// covering the padding boundaries in `finish()`: 55/56 (one block vs two),
    /// 63/64/65 (a full buffered block), 119/120 and 127/128/129.
    ///
    /// Generated independently of this code, on macOS, with
    /// `head -c $n pattern | shasum -a 256`, cross-checked against
    /// `openssl dgst -sha256` (all 131 agreed), where `pattern` was written by
    /// `bytes((i*7+1)%256 for i in range(130))` in Python.
    const BOUNDARY_DIGESTS: [&str; 131] = [
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", // 0
        "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a", // 1
        "9e6282e4f25e370ce617e21d6fe265e88b9e7b8682cf00059b9d128d9381f09d", // 2
        "ce562c676ea7aba9812f54db146b0096011bf3e423edfb0a6f8fb2b02674a801", // 3
        "f4c9670a5c59e00b89d9dd11db514903a33cd1dcc6598a0cfb34a56f708c9b8f", // 4
        "9bd7abda3405cf987d067870c68ac01c5371272b8df8389e10b403556b2604cf", // 5
        "6c113ad1fc77d52f6928ec89e86211aebf09bfbf2862ca345f3322a6c89add29", // 6
        "23e3e1234d677e795f66e80526eac12ae5bb34a972177b971f61602478af6a80", // 7
        "5caa048e02e52030c521f8966a8e1f1a233dd165dbe326c369362a6227d7881f", // 8
        "53dca2985185e102545c0e42c5d8c20a3a775f77e1581bc01eeb4a0035a37fb8", // 9
        "75ff8cdbed69eeccd0456df4dbaef1cfab5589eba7c739a1cc8cd3a3fac1182a", // 10
        "edf74f3c2c9718eb284bf31dc5aee28adc88a3a4eded91f1857701d0b226d6e5", // 11
        "a86e181d493d9cd0cef42e540d571b5c315a2dbac1f0e04adf9b7d4e7e505a09", // 12
        "b356740e8072460b82f7bba522879243a5299d210d89bb9ba5cf06babfa21c66", // 13
        "734223d424771fd9d247b92e4d1e2d8a35865fcd65c5a67add5af8c365de650a", // 14
        "ff2d70d52fdf789804aca62f3e11ee56e15f2f4e8d0b4a155b4ae8616c431868", // 15
        "3456812eddd9d434d710db2158d4a600997d37835a2b4a80d0b84b029eda1003", // 16
        "a28afa672e7d13780abb411f3e530fa9b6150a269067b10a9970c31cfe08acc1", // 17
        "56f6cb11d014a67b66fa2871a7a07ffc7a55fa7d15b03b787e7aa5f2029d9a00", // 18
        "f503b69ba27d9afa6cc226b54286281eed293108b28d5688c8d2e68cc02b0f91", // 19
        "c0c1e4041fec91793f2ae907d8a09f4c225b4595de9178d79235cb8362333a90", // 20
        "8a5c1aca954cef7632be62dee9ff8f8df6e535e877349013d7311cb927f6ff55", // 21
        "ffd8aec822956bc0d215153e013bcd954dcf4fb518dc7e2c21781ec65b14d2d9", // 22
        "ed7200da21a22e54cba5832c997384e74696ef6e826bb61c4935c935b70a9751", // 23
        "993ea42e5263f19fd69652835ae9ef949e9cca5289ec29dcd7d3aa2763afc78e", // 24
        "0a8109a7401a5de2dbadd377a9a7e4ca9e2ccc3e2defc0faa11af87d71c42fda", // 25
        "f14028c478d136193fbee54f95b098dc76aef50829e31c5975f64b22d753e1bd", // 26
        "ff59a40d30586d89564b3b9b8a45a26e762ffc2e9d3028124c4a175702af8838", // 27
        "3146c75ad4aa41ac2003b7c363800dbf03c79374725fcfc9f6278f956fee4c31", // 28
        "8204f3162828f3f51ae96db756b868e1120ba7a8bc4649ed3900a32fb1180fe5", // 29
        "c83eb9bed1ad70b4ddcea784d9d418dbc57086fd67ff639e5020fcdc8087aff0", // 30
        "f47d790dd576502108aa0536f0ff49ef0ad7d6fdb69c7894f9f3cdd58736523a", // 31
        "8c731c199b130cb096cae9129de0680c61e60606ed5c3ba72ebac58ad6af6df3", // 32
        "473311950ad9d61883dae98aa25d2939bb366f6023d8ccb33c8da3168e64c639", // 33
        "3d7848be3860e55d4ebb46fc9c70579b16eba4511df84497e54898cc8eec9955", // 34
        "3f80b9ef04f000c4f9c8945c8694229b42e75e55d8ec5b67d507f6e79b05f839", // 35
        "3c3bf2158fdb20122cc3ce1c36958903ca4897d402d45834eb648b71f2640343", // 36
        "b6e4ca938d7f2163fefe50251baaa830407cfdd72895232daf4ec0d50eb149c7", // 37
        "74805b6d70f7cfc31a1b9ca07ba7a9a060888b679aff651ce9392c989a0f3157", // 38
        "5564f16c6d0058cd6120aff206d15e5d2d414b8b72d60a575215d4183f9f95c7", // 39
        "9b2cc135a6edd01f77367bd6ea22ed75b50d6b802b32ea35b5a09b0a8d99d335", // 40
        "8bad924a58de3637a6b2aa9c617afb5095fe43998f36d2d8ea1ffa6efc7fb489", // 41
        "2f66f3ebd311431f88c8f2c81d52b53ae66bf9705d9011c555e3544135253780", // 42
        "2531ef3197c7f009c6b00dac5f9c9c32352238767eab6ee5131b489a7d2542a8", // 43
        "c1383a6004096aee3ab5837a11d57112a8253de02c64e30f580f847206fa899a", // 44
        "bcc5623ceeeca13f1e9941a0b01293d406141719aa61f67c0ca7abd1f5909b57", // 45
        "17cb401d52249a33a1b9a2ade8fdb6a8fa894478c1b2fe71e5b374bb9176b73b", // 46
        "7d28b065178c1ff33b2aee130414deb19bb966b1d2df27b789bc0f691dc7931c", // 47
        "9cf40ee1d57c7ce9ceebcc477b548c089f2f18945f68c2acaf8b6c4ad38b0412", // 48
        "7bdb80f9f8c8a398bc70bafc3724d0c91ee1ddb0bd2f491cf9d356ca9307f0f8", // 49
        "c4a2bc1979d78695e8c87fc96a74793e8dfb20e9ae9cc22f4b41ce7e37e833ff", // 50
        "e94202a8a0a1c13bb654c4e27fbae8c2916d4c05d18e97f59da8e00cd84182fe", // 51
        "71ba243afbd76efabd9f2cee20aca1bb1f4a5ce94b506ce8cada4c4c2b6cc1af", // 52
        "6c9ecb608939e78801a79714d5d31b0d131623f14b165b2af7b2ce24cf6f8397", // 53
        "5556be1c204be4be0266d24d065b006343ecd327c0adbc0c98db3a359eb7a9b6", // 54
        "16fa57a0a3423a715d594516339f36189d6b5f93754a9714fef202616a9fabfe", // 55
        "c37b44e5f1b18554b36966f4f8e08bfbf3164c4b6c10374d12d89850892073c5", // 56
        "12b234922502022f755ab8550a3d4e202ad39c81d961a4f59ec39d5fd83d15a7", // 57
        "63c523ac29cd511c86fecf220a805201451a01a014b0c53a0cf9f9521eba55de", // 58
        "8941b945f678cbd746cfac4f5fd4e13a5819c723ddb3f59651ef7ac9f785305c", // 59
        "8215da63b76b62806a5cb6bd76c55f95e334e36f48a507a5603bcacf08047199", // 60
        "371e309b8b6b9f0d18bb605cdd2824ee4ebada4e09338495ce6db7acfd771db0", // 61
        "93bd1e900ea389c9c8144424f2f83a5a4dbcb7c1bbe5cda4d940f66ec4de80d3", // 62
        "bbba992d2c85af960fb2987a1fd05e0aa82a3db3c740dd8982a9e273b75e36a3", // 63
        "66bd4633ed6f71c4ecfa4763bf7ba1c8ec7612de9aa6c0578a7b675207c71e0b", // 64
        "9f7dc47107b750a1f3d35db5d9547f24ef40da5b731b9540d4f43710a154f6c9", // 65
        "2be1a5764bd98d01a729b13d558ceec2d382f649783369983de4f7dde2854ad9", // 66
        "6d6dd48abbda404037e8a3678c7a407511298d72036d1bbda681f4f37b41abab", // 67
        "cb5a849deabb6e67e05cda7f51e0f57d382d6f708fb0cd3e192da0de59cd4322", // 68
        "70019dc9e6af73abfb7da271e6875f40bc24f9c01de079c0de1f59c98dea813f", // 69
        "1d1788cf5b3bb5daa56498ec802c29cd3b12fb488b11649bd0af8fffc71bf23b", // 70
        "6cf1df70a98782084d8c715381b1f35c30eca3ae6bab2284d2199bd13910f94d", // 71
        "284d694a934cf21f04ded24724e8bda357cbddb869c8fdd4506b82b678aedaab", // 72
        "9d0afa84063085f1912f9863d02121a6d0fe7ad68bfee905942e641d51788476", // 73
        "182a06b105da0d284eb4dda4351b220e70802cbb055a04f8df6dbd3c7eb19b6b", // 74
        "cc3cff25ffa20a739cf439a143d3e9b308582d250203e06245457fc292257fdf", // 75
        "3afffd000685fa088cfcb4c09b26a0b18f66fa051d9234ea545344486a610c39", // 76
        "6de8ea1f7015d023d8abb26051e52da5e7c77a80ca6146b40d49eae92984a13f", // 77
        "5eeb1f676a61006f09bb3b692df052ca475914f6256d05f5bb8f35d7f6a85369", // 78
        "1632b36783f7da4d7acec8265e3627a95ac2a3c438c9f85c577c87c48104e09e", // 79
        "15fd009d422e64dc0725198e7c21e2770d8b4eff1c85cfe179a891f1f0d70bec", // 80
        "54ef94fdaf9a87eafa2ab68c192a1ffeeb07f8f66cb344d9c4184b26ef3280f5", // 81
        "de5411ac6a95226e74a9a671686ccefff9842ccbb062c516591d5312d70fec9a", // 82
        "d05079c1ae16d5f1143c1b08e6513e5d1f7ab2cda5b8c1520fcb08916ff74237", // 83
        "d6c7a60ab023bf58bb5d5a8147ca76eea5d0f0c219d48c2fd108d658ea404cca", // 84
        "02213270d717fbbd863a80427ba16e86054f80c56b2164bbf0d41526dc64ca64", // 85
        "43947d20db2548865a851d6c8e9055abbdc135a7ecbaca07af84f52622c08b12", // 86
        "66d9298700790d55225b07935c430aa3b01ebc40d9348c4cd697830354229eb4", // 87
        "2dbf2f57adbf61059316c1bbb35e45f1d17b991abe8c919ee2efeae4d24ac7e1", // 88
        "3969a8191ef18cf8f535851e5ae50ec86d3b6289ca0114d6745be1f77900e6ba", // 89
        "a9be0f57faacbf0d26de6970afe57b12bde39b4beb148ff589b83a6acaaa876a", // 90
        "9d56af4acfef9536f15f801f59fad38d62969afe19d69e88a550577cb4ad2b91", // 91
        "0a6ec85ad410a0313754d22cfc8c2f93f59053c374698ea0bda0028b9b7a75f1", // 92
        "c976ab6d7c0ce406099ccd4bd8b34839e36dce989be0ce71ec8f400bcf3c48de", // 93
        "c49df7fb969ba63244e8e52e48f48332bb75e1226fcb76d68f1d780edadf697f", // 94
        "c825c21b58a36bf9c81c240aecdb3bb350691795c0b262eb83e350f7804a622c", // 95
        "52e36ce624c8dc22735566b84e1e7ed904fc7a14f302727b8fbe462cd71e0999", // 96
        "d0c2e9b5d9086a5aee077646b0cdeea5ef95944a158b597876f9e113a38a9a06", // 97
        "c0a9f8792d4bb67f3e9d7c0121bd1fae53096f644546e4bb147836a91a834db1", // 98
        "f1a811942f47b13772165561ea64b9e6d9309af3af83e38278d9d322314fded9", // 99
        "4303a0db0805657f94896cbe70712284dd3d74b1324a92b677b792b63b5d7538", // 100
        "9d77f4416fbb1f71007e64a292e1cb83b1ed4cd93ceddc36acfc3d9d7e647133", // 101
        "9838cbcae4bf32d6ebe459134f02af29f8a92459af85e568d56dffa2ebb625ec", // 102
        "bebee0a14ee5eb0cc99f243acb45bce878170901896c049904430524b0935bef", // 103
        "7de3aec665987c0b9040222616ef519b5fc50944c384d48d54d6594bc29f845d", // 104
        "6ae404ab5f1e3f05c677fbdecbe967b06d7cda6a1ab28a1fcd6f53a3a940ae6e", // 105
        "e489fceb9bba96dc6bfefde3af8de5e479a1c95af13b946364d821a5a3ddeed2", // 106
        "c56a243f8054253bbaddc2267a1e42cdee9f905d6fc7a4622647d44a08289b16", // 107
        "f6ccc522a71e9dc51b12a4ae6eafc0456c6ead3011c9b7efff51624c338fc952", // 108
        "e67472cb15a8b42e5304265c67175fceea56e1eaf6699038d11d535af3f6eaf2", // 109
        "4c50c81c985a99ba1d233fcbde98217b0850eb6a67921a4441b97dad21fe5537", // 110
        "1796402e8fc028fc1d3a61e3e4cac839872632c2d7b381cbd802d0768f5cfd13", // 111
        "e19b57d2618b2898cb915bf2be9bf558f761faa651cd6c5f892610750714dea4", // 112
        "e6ace23c9045727ccf80b413fece760ed8ce384d3e89d07f971b0e786e389b29", // 113
        "cfb383f7e7a34ef64fd03e8f4d39ab6709b87cfa77188aa9d6e8c7b3f03f0c6c", // 114
        "7b8cbdcc6578137379b6d581d8696505a3c55c53888e3dbb50ec5e1dec0e8fd8", // 115
        "29145520e85dba69be064de150093cdb8200362ee60df8e7e3ebc6f40af147ac", // 116
        "a52ff4c6c1b82df939c714a8fe676c1cc2dd925313957cd6fa9ffa70379f7ee4", // 117
        "d63612beb497b3917ef96ba278b6eff6652f0ece634a810366f905b3a5f3c732", // 118
        "a3ed307b730fa77c07531300c6e4a282330011d4d4caf6bb7b63ae05950f4b66", // 119
        "8e3b15d9fea7472655aa069620b7f8c2e55ee1499f763200a7515fe826e99d20", // 120
        "a8320d9736c8a5a7445c87c4d18c0da2fa8fb3b323ed8318c724a5b3e230f87b", // 121
        "20f614857fa0885d8fbfbc82eec0dac0eb1e27efb2bbb06e581bcd54987a2d51", // 122
        "505dce895b498695c909f9ee0575e4929f53ef32b4e23eb135a0da645435a119", // 123
        "bcf1a42a8454ad8e2f860342b534157bc4c16734307f6a3dd66183d4c1e83652", // 124
        "ac550c3fd5f8d4bbbadb60add61d3674efc8c6ebfb0bb59821ae0f253164a4b0", // 125
        "41fd5b531ba829992623321149f09e5909ca52c45a89e92f935eff150e677192", // 126
        "44480fb9672845177f5368a08b69ea263275f2a5ec42e06a933370fe0d2968a4", // 127
        "e462c130fef8c97e34f7dc3ff3ad2f8b3533ab849af21c10531552a2852387a4", // 128
        "aa7ea4e8bf89146aeb67ff195fd8182a0e504c9d580aa7af8d2c862e14b98405", // 129
        "a234574a31eaa90ec795dcd0a21f34674b8ae54f47912ae62cb0571c1cb6b9ab", // 130
    ];

    fn boundary_pattern() -> Vec<u8> {
        (0..130u32).map(|i| ((i * 7 + 1) % 256) as u8).collect()
    }

    #[test]
    fn padding_boundaries_one_shot() {
        let data = boundary_pattern();
        for (n, want) in BOUNDARY_DIGESTS.iter().enumerate() {
            assert_eq!(hex(&digest(&data[..n])), *want, "length {n}");
        }
    }

    #[test]
    fn padding_boundaries_byte_at_a_time() {
        let data = boundary_pattern();
        for (n, want) in BOUNDARY_DIGESTS.iter().enumerate() {
            let mut h = Sha256::new();
            for b in &data[..n] {
                h.update(std::slice::from_ref(b));
            }
            assert_eq!(hex(&h.finish()), *want, "length {n}");
        }
    }

    #[test]
    fn incremental_equals_one_shot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect();
        for split in [0, 1, 63, 64, 65, 127, 5000, 9999] {
            let mut h = Sha256::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.finish(), digest(&data), "split {split}");
        }
    }
}
