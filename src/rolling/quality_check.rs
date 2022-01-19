use crate::rolling::kseq_iterator::RollingKseqImpl;

pub struct RollingQualityCheck {
    prob_log: u64,
}

// const MIN_SCORE: usize = b'!' as usize;
pub static SCORES_INDEX: [u64; 256] = [
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    4398046510080,
    737473076,
    464847996,
    324337075,
    236739489,
    177262468,
    134891584,
    103780256,
    80466606,
    62744290,
    49131731,
    38595128,
    30392030,
    23977377,
    18944183,
    14984533,
    11863057,
    9398386,
    7449869,
    5907889,
    4686674,
    3718902,
    2951602,
    2343013,
    1860159,
    1476970,
    1172816,
    931360,
    739653,
    587432,
    466553,
    370558,
    294320,
    233772,
    185682,
    147486,
    117149,
    93052,
    73912,
    58709,
    46634,
    37042,
    29423,
    23371,
    18564,
    14746,
    11713,
    9304,
    7390,
    5870,
    4663,
    3704,
    2942,
    2337,
    1856,
    1474,
    1171,
    930,
    739,
    587,
    466,
    370,
    294,
    233,
    185,
    147,
    117,
    93,
    73,
    58,
    46,
    37,
    29,
    23,
    18,
    14,
    11,
    9,
    7,
    5,
    4,
    3,
    2,
    2,
    1,
    1,
    1,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
];

// fn compute_logprob() -> [u64; 256] {
//     let mut result = [0; 256];
//
//     let mut i = MIN_SCORE;
//     while i < 256 {
//         let qual_idx = i - MIN_SCORE;
//         let err_prob = (10.0 as f64).powf(-(qual_idx as f64) / 10.0);
//         let corr_prob = 1.0 - err_prob;
//         let logval = min(
//             (u32::MAX as u64) * 1024,
//             (-corr_prob.log10() * (LOGPROB_MULTIPLIER as f64)) as u64,
//         );
//         result[i] = logval;
//         i += 1;
//     }
//
//     result
// }

pub const LOGPROB_MULTIPLIER: u64 = 1073741824;

impl RollingQualityCheck {
    pub fn new() -> RollingQualityCheck {
        RollingQualityCheck { prob_log: 0 }
    }

    pub fn get_log_for_correct_probability(prob: f64) -> u64 {
        (-prob.log10() * (LOGPROB_MULTIPLIER as f64)) as u64
    }
}

impl RollingKseqImpl<u8, u64> for RollingQualityCheck {
    #[inline(always)]
    fn clear(&mut self, _ksize: usize) {
        self.prob_log = 0
    }

    #[inline(always)]
    fn init(&mut self, _index: usize, base: u8) {
        //1.0 - (10.0 as f64).powf(-(0.1 * ((*qb as f64) - 33.0)));
        self.prob_log += unsafe { *SCORES_INDEX.get_unchecked(base as usize) };
    }

    #[inline(always)]
    fn iter(&mut self, _index: usize, out_base: u8, in_base: u8) -> u64 {
        self.prob_log += unsafe { *SCORES_INDEX.get_unchecked(in_base as usize) };
        let result = self.prob_log;
        self.prob_log -= unsafe { *SCORES_INDEX.get_unchecked(out_base as usize) };
        result
    }
}
