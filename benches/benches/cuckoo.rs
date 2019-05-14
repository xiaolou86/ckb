use ckb_pow::Cuckoo;
use criterion::{criterion_group, criterion_main, Criterion};

const TESTSET: [([u8; 80], [u32; 8]); 3] = [
    (
        [
            238, 237, 143, 251, 211, 26, 16, 237, 158, 89, 77, 62, 49, 241, 85, 233, 49, 77, 230,
            148, 177, 49, 129, 38, 152, 148, 40, 170, 1, 115, 145, 191, 44, 10, 206, 23, 226, 132,
            186, 196, 204, 205, 133, 173, 209, 20, 116, 16, 159, 161, 117, 167, 151, 171, 246, 181,
            209, 140, 189, 163, 206, 155, 209, 157, 110, 2, 79, 249, 34, 228, 252, 245, 141, 27, 9,
            156, 85, 58, 121, 46,
        ],
        [1, 12, 23, 27, 31, 48, 50, 60],
    ),
    (
        [
            146, 101, 131, 178, 127, 39, 4, 255, 226, 74, 32, 146, 158, 0, 206, 120, 198, 96, 227,
            140, 133, 121, 248, 27, 69, 136, 108, 226, 11, 47, 250, 27, 3, 94, 249, 46, 158, 71,
            83, 205, 196, 206, 65, 31, 158, 62, 7, 45, 235, 234, 165, 137, 253, 210, 15, 224, 232,
            233, 116, 214, 231, 234, 47, 3, 64, 250, 246, 80, 161, 51, 61, 153, 217, 101, 82, 189,
            62, 247, 194, 3,
        ],
        [16, 26, 29, 33, 39, 43, 44, 54],
    ),
    (
        [
            24, 75, 179, 121, 98, 241, 250, 124, 100, 197, 125, 237, 29, 128, 222, 12, 134, 5, 241,
            148, 87, 86, 159, 53, 217, 6, 202, 87, 71, 169, 8, 6, 202, 47, 50, 214, 18, 68, 84,
            248, 105, 201, 162, 182, 95, 189, 145, 108, 234, 173, 81, 191, 109, 56, 192, 59, 176,
            113, 85, 75, 254, 237, 161, 177, 189, 22, 219, 131, 24, 67, 96, 12, 22, 192, 108, 1,
            189, 243, 22, 31,
        ],
        [1, 15, 20, 22, 39, 41, 52, 56],
    ),
];

fn bench(c: &mut Criterion) {
    c.bench_function("bench_solve", |b| {
        let cuckoo = Cuckoo::new(6, 8);
        b.iter(|| {
            for _ in 0..100 {
                for (message, _) in TESTSET.iter() {
                    cuckoo.solve(message).unwrap();
                }
            }
        })
    });

    c.bench_function("bench_verify", |b| {
        let cuckoo = Cuckoo::new(6, 8);
        b.iter(|| {
            for _ in 0..100 {
                for (message, proof) in TESTSET.iter() {
                    cuckoo.verify(message, proof);
                }
            }
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
