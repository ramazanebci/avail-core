use avail_core::{AppExtrinsic, BlockLengthColumns, BlockLengthRows, DataLookup};
use core::num::NonZeroU32;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use dusk_plonk::prelude::BlsScalar;
use kate::{
	com::{Cell, *},
	metrics::IgnoreMetrics,
	Seed, Serializable as _,
};
use kate_recovery::{
	com::reconstruct_extrinsics,
	commitments,
	data::{self, DataCell},
	matrix::Position,
	proof, testnet,
};
use nalgebra::DMatrix;
use rand::{prelude::IteratorRandom, Rng, SeedableRng};
use rand_chacha::ChaChaRng;
use sp_arithmetic::{traits::SaturatedConversion, Percent};

const XTS_JSON_SETS: &str = include_str!("reconstruct.data.json");

#[rustfmt::skip]
fn load_xts() -> Vec<Vec<AppExtrinsic>> {
	serde_json::from_str(XTS_JSON_SETS).expect("Autogenerated Json file .qed")
}

fn sample_cells_from_matrix(matrix: &DMatrix<BlsScalar>, columns: Option<&[u16]>) -> Vec<DataCell> {
	fn random_indexes(length: usize, seed: Seed) -> Vec<usize> {
		// choose random len/2 (unique) indexes
		let mut idx = (0..length).collect::<Vec<_>>();
		let mut chosen_idx = Vec::<usize>::new();
		let mut rng = ChaChaRng::from_seed(seed);

		for _ in 0..length / 2 {
			let i = rng.gen_range(0..idx.len());
			let v = idx.remove(i);
			chosen_idx.push(v);
		}
		chosen_idx
	}
	const RNG_SEED: Seed = [42u8; 32];

	let (rows, cols) = matrix.shape();
	let cols = u16::try_from(cols).unwrap();
	let indexes = random_indexes(rows, RNG_SEED);

	(0u16..cols)
		.filter(|col_idx| match &columns {
			None => true,
			Some(allowed) => allowed.contains(col_idx),
		})
		.flat_map(|col_idx| {
			let col_view = matrix.column(col_idx.into()).data.into_slice();

			indexes
				.iter()
				.map(|row_idx| {
					let row_pos = u32::try_from(*row_idx).unwrap();
					let position = Position::new(row_pos, col_idx);
					debug_assert!(*row_idx < col_view.len());
					let data = col_view[*row_idx].to_bytes();
					DataCell::new(position, data)
				})
				.collect::<Vec<_>>()
		})
		.collect()
}

fn random_cells(
	max_cols: BlockLengthColumns,
	max_rows: BlockLengthRows,
	percents: Percent,
) -> Vec<Cell> {
	let max_cols = max_cols.into();
	let max_rows = max_rows.into();

	let rng = &mut ChaChaRng::from_seed([0u8; 32]);
	let amount: usize = percents
		.mul_ceil::<u32>(max_cols * max_rows)
		.saturated_into();

	(0..max_cols)
		.flat_map(move |col| {
			(0..max_rows).map(move |row| Cell::new(BlockLengthRows(row), BlockLengthColumns(col)))
		})
		.choose_multiple(rng, amount)
}

fn bench_reconstruct(c: &mut Criterion) {
	let xts_sets = load_xts();

	let mut group = c.benchmark_group("reconstruct from xts");
	for xts in xts_sets.into_iter() {
		let size = xts
			.iter()
			.map(|app| app.opaque.len())
			.sum::<usize>()
			.try_into()
			.unwrap();
		group.throughput(Throughput::Bytes(size));
		group.sample_size(10);
		group.bench_with_input(BenchmarkId::from_parameter(size), &xts, |b, xts| {
			b.iter(|| reconstruct(xts.as_slice()))
		});
	}
	group.finish();
}

fn reconstruct(xts: &[AppExtrinsic]) {
	let metrics = IgnoreMetrics {};
	let (layout, commitments, dims, matrix) = par_build_commitments(
		BlockLengthRows(64),
		BlockLengthColumns(16),
		unsafe { NonZeroU32::new_unchecked(32) },
		xts,
		Seed::default(),
		&metrics,
	)
	.unwrap();

	let columns = sample_cells_from_matrix(&matrix, None);
	let extended_dims = dims.try_into().unwrap();
	let lookup = DataLookup::from_id_and_len_iter(layout.into_iter()).unwrap();
	let reconstructed = reconstruct_extrinsics(&lookup, extended_dims, columns).unwrap();
	for ((app_id, data), xt) in reconstructed.iter().zip(xts) {
		assert_eq!(app_id.0, *xt.app_id);
		assert_eq!(data[0].as_slice(), &xt.opaque);
	}

	let dims_cols: u32 = dims.cols().into();
	let public_params = testnet::public_params(usize::try_from(dims_cols).unwrap());
	for cell in random_cells(dims.cols(), dims.rows(), Percent::one()) {
		let row: u32 = cell.row.into();

		let proof = build_proof(&public_params, dims, &matrix, &[cell], &metrics).unwrap();
		assert_eq!(proof.len(), 80);

		let col: u16 = cell
			.col
			.0
			.try_into()
			.expect("`random_cells` function generates a valid `u16` for columns");
		let position = Position { row, col };
		let cell = data::Cell {
			position,
			content: proof.try_into().unwrap(),
		};

		let extended_dims = dims.try_into().unwrap();
		let commitment = commitments::from_slice(&commitments).unwrap()[row as usize];
		let verification = proof::verify(&public_params, extended_dims, &commitment, &cell);
		assert!(verification.is_ok());
		assert!(verification.unwrap());
	}
}

criterion_group! { benches, bench_reconstruct }
criterion_main!(benches);
