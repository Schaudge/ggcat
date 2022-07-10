use crate::processor::{KmersProcessorInitData, KmersTransformProcessor};
use crate::reads_buffer::ReadsBuffer;
use crate::resplitter::{KmersTransformResplitter, ResplitterInitData};
use crate::rewriter::{KmersTransformRewriter, RewriterInitData};
use crate::{KmersTransformContext, KmersTransformExecutorFactory, KmersTransformPreprocessor};
use config::{
    DEFAULT_OUTPUT_BUFFER_SIZE, DEFAULT_PREFETCH_AMOUNT, KEEP_FILES, MAXIMUM_JIT_PROCESSED_BUCKETS,
    MIN_BUCKET_CHUNKS_FOR_READING_THREAD, PACKETS_PRIORITY_DEFAULT, USE_SECOND_BUCKET,
};
use io::compressed_read::CompressedReadIndipendent;
use io::concurrent::temp_reads::creads_utils::CompressedReadsBucketHelper;
use io::concurrent::temp_reads::extra_data::SequenceExtraDataTempBufferManagement;
use minimizer_bucketing::counters_analyzer::BucketCounter;
use parallel_processor::buckets::readers::async_binary_reader::{
    AsyncBinaryReader, AsyncReaderThread,
};
use parallel_processor::counter_stats::counter::{AtomicCounter, SumMode};
use parallel_processor::counter_stats::declare_counter_i64;
use parallel_processor::execution_manager::executor::{
    AsyncExecutor, ExecutorAddressOperations, ExecutorReceiver,
};
use parallel_processor::execution_manager::executor_address::ExecutorAddress;
use parallel_processor::execution_manager::memory_tracker::MemoryTracker;
use parallel_processor::execution_manager::objects_pool::{PoolObject, PoolObjectTrait};
use parallel_processor::execution_manager::packet::{Packet, PacketTrait, PacketsPool};
use parallel_processor::memory_fs::RemoveFileMode;
use parallel_processor::utils::replace_with_async::replace_with_async;
use std::cmp::{max, min, Reverse};
use std::collections::{BinaryHeap, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use utils::track;

pub struct KmersTransformReader<F: KmersTransformExecutorFactory> {
    _phantom: PhantomData<F>,
}

pub struct InputBucketDesc {
    pub(crate) path: PathBuf,
    pub(crate) sub_bucket_counters: Vec<BucketCounter>,
    pub(crate) resplitted: bool,
    pub(crate) rewritten: bool,
    pub(crate) used_hash_bits: usize,
}

impl PoolObjectTrait for InputBucketDesc {
    type InitData = ();

    fn allocate_new(_init_data: &Self::InitData) -> Self {
        Self {
            path: PathBuf::new(),
            sub_bucket_counters: Vec::new(),
            resplitted: false,
            rewritten: false,
            used_hash_bits: 0,
        }
    }

    fn reset(&mut self) {
        self.resplitted = false;
        self.sub_bucket_counters.clear();
    }
}
impl PacketTrait for InputBucketDesc {
    fn get_size(&self) -> usize {
        1 // TODO: Maybe specify size
    }
}

static ADDR_WAITING_COUNTER: AtomicCounter<SumMode> =
    declare_counter_i64!("kt_addr_wait_reader", SumMode, false);

static PACKET_WAITING_COUNTER: AtomicCounter<SumMode> =
    declare_counter_i64!("kt_packet_wait_reader", SumMode, false);

static START_PACKET_ALLOC_COUNTER: AtomicCounter<SumMode> =
    declare_counter_i64!("kt_packet_alloc_reader_startup", SumMode, false);

static PACKET_ALLOC_COUNTER: AtomicCounter<SumMode> =
    declare_counter_i64!("kt_packet_alloc_reader", SumMode, false);

struct BucketsInfo {
    reader: AsyncBinaryReader,
    concurrency: usize,
    addresses: Vec<ExecutorAddress>,
    register_addresses: Vec<ExecutorAddress>,
    buckets_remapping: Vec<usize>,
    second_buckets_log_max: usize,
    file_size: usize,
    used_hash_bits: usize,
}

impl<F: KmersTransformExecutorFactory> KmersTransformReader<F> {
    fn compute_buckets(
        global_context: &KmersTransformContext<F>,
        file: Packet<InputBucketDesc>,
    ) -> BucketsInfo {
        let second_buckets_log_max = min(
            file.sub_bucket_counters.len().log2() as usize,
            global_context.max_second_buckets_count_log2,
        );

        let reader = AsyncBinaryReader::new(
            &file.path,
            true,
            RemoveFileMode::Remove {
                remove_fs: file.rewritten || !KEEP_FILES.load(Ordering::Relaxed),
            },
            DEFAULT_PREFETCH_AMOUNT,
        );

        let second_buckets_max = 1 << second_buckets_log_max;

        let mut buckets_remapping = vec![0; second_buckets_max];

        let mut queue = BinaryHeap::new();
        queue.push((Reverse(0), 0, false));

        let mut sequences_count = 0;

        let mut bucket_sizes: VecDeque<_> = (0..(1 << second_buckets_log_max))
            .map(|i| {
                sequences_count += file.sub_bucket_counters[i].count;
                (file.sub_bucket_counters[i].clone(), i)
            })
            .collect();

        let file_size = reader.get_file_size();

        let sequences_size_ratio =
            file_size as f64 / (sequences_count * global_context.k as u64) as f64 * 2.67;

        bucket_sizes.make_contiguous().sort();

        let unique_estimator_factor = (sequences_size_ratio * sequences_size_ratio * 3.0).min(1.0);

        let mut has_outliers = false;

        while bucket_sizes.len() > 0 {
            let buckets_count = queue.len();
            let mut smallest_bucket = queue.pop().unwrap();

            let biggest_sub_bucket = bucket_sizes.pop_back().unwrap();

            // Alloc a new bucket
            if (smallest_bucket.2 == biggest_sub_bucket.0.is_outlier)
                && smallest_bucket.0 .0 > 0
                && (biggest_sub_bucket.0.count + smallest_bucket.0 .0) as f64
                    * unique_estimator_factor
                    > global_context.min_bucket_size as f64
            {
                // Restore the sub bucket
                bucket_sizes.push_back(biggest_sub_bucket);

                // Push the current bucket
                queue.push(smallest_bucket);

                // Add the new bucket
                queue.push((Reverse(0), buckets_count, false));
                continue;
            }

            // Assign the sub-bucket to the current smallest bucket
            smallest_bucket.0 .0 += biggest_sub_bucket.0.count;
            smallest_bucket.2 |= biggest_sub_bucket.0.is_outlier;
            has_outliers |= biggest_sub_bucket.0.is_outlier;
            buckets_remapping[biggest_sub_bucket.1] = smallest_bucket.1;
            queue.push(smallest_bucket);
        }

        let mut addresses: Vec<_> = vec![None; queue.len()];
        let mut register_addresses = Vec::new();
        let mut dbg_counters: Vec<_> = vec![0; queue.len()];

        let mut jit_executors = 0;

        let rewriter_address =
            KmersTransformRewriter::<F>::generate_new_address(RewriterInitData {
                buckets_count: queue.len(),
                buckets_hash_bits: second_buckets_max.log2() as usize,
                used_hash_bits: file.used_hash_bits,
            });

        let mut pushed_rewriter = false;

        for (count, index, outlier) in queue.into_iter() {
            dbg_counters[index] = count.0;
            addresses[index] = Some(if outlier {
                // println!("Sub-bucket {} is an outlier with size {}!", index, count.0);
                let new_address =
                    KmersTransformResplitter::<F>::generate_new_address(ResplitterInitData {
                        bucket_size: count.0 as usize,
                    });
                register_addresses.push(new_address.clone());
                new_address
            } else {
                if !has_outliers
                    && (file.rewritten
                        || jit_executors
                            < max(
                                global_context.compute_threads_count,
                                MAXIMUM_JIT_PROCESSED_BUCKETS,
                            ))
                {
                    jit_executors += 1;
                    let new_address = KmersTransformProcessor::<F>::generate_new_address(
                        KmersProcessorInitData {
                            sequences_count: count.0 as usize,
                            sub_bucket: index,
                            bucket_path: file.path.clone(),
                        },
                    );
                    register_addresses.push(new_address.clone());
                    new_address
                } else {
                    if !pushed_rewriter {
                        register_addresses.push(rewriter_address.clone());
                        pushed_rewriter = true;
                    }
                    rewriter_address.clone()
                }
            });
        }

        let addresses: Vec<_> = addresses.into_iter().map(|a| a.unwrap()).collect();

        let threads_ratio =
            global_context.compute_threads_count as f64 / global_context.read_threads_count as f64;

        let addr_concurrency = max(1, (addresses.len() as f64 / threads_ratio + 0.5) as usize);
        let chunks_concurrency = max(
            1,
            reader.get_chunks_count() / MIN_BUCKET_CHUNKS_FOR_READING_THREAD,
        );

        let concurrency = min(
            min(4, global_context.read_threads_count),
            min(addr_concurrency, chunks_concurrency),
        );

        //     println!(
        //     "File:{}\nChunks {} concurrency: {} REMAPPINGS: {:?} // {:?} // {:?} RATIO: {:.2} ADDR_COUNT: {}",
        //     file.path.display(),
        //     reader.get_chunks_count(),
        //     concurrency,
        //     &buckets_remapping,
        //     dbg_counters,
        //     file.sub_bucket_counters
        //         .iter()
        //         .map(|x| x.count)
        //         .collect::<Vec<_>>(),
        //     sequences_size_ratio,
        //     addresses.len()
        // );

        BucketsInfo {
            reader,
            concurrency,
            addresses,
            register_addresses,
            buckets_remapping,
            second_buckets_log_max,
            file_size,
            used_hash_bits: file.used_hash_bits,
        }
    }

    #[instrumenter::track]
    async fn read_bucket(
        global_context: &KmersTransformContext<F>,
        ops: &ExecutorAddressOperations<'_, Self>,
        bucket_info: &BucketsInfo,
        async_reader_thread: Arc<AsyncReaderThread>,
        packets_pool: Arc<PoolObject<PacketsPool<ReadsBuffer<F::AssociatedExtraData>>>>,
    ) {
        if bucket_info.reader.is_finished() {
            return;
        }

        let mut buffers = Vec::with_capacity(bucket_info.addresses.len());

        track!(
            {
                for _ in 0..bucket_info.addresses.len() {
                    buffers.push(packets_pool.alloc_packet().await);
                }
            },
            START_PACKET_ALLOC_COUNTER
        );

        let preprocessor = F::new_preprocessor(&global_context.global_extra_data);

        let global_extra_data = &global_context.global_extra_data;

        let has_single_addr = bucket_info.addresses.len() == 1;

        let mut items_iterator = bucket_info
            .reader
            .get_items_stream::<CompressedReadsBucketHelper<
                F::AssociatedExtraData,
                F::FLAGS_COUNT,
                { USE_SECOND_BUCKET },
            >>(
                async_reader_thread.clone(),
                Vec::new(),
                F::AssociatedExtraData::new_temp_buffer(),
            );

        let mut buckets_mults = vec![0; 1 << bucket_info.second_buckets_log_max];

        while let Some((read_info, extra_buffer)) = items_iterator.next() {
            let orig_bucket = preprocessor.get_sequence_bucket(
                global_extra_data,
                &read_info,
                bucket_info.used_hash_bits,
                bucket_info.second_buckets_log_max,
            ) as usize;

            let bucket = if has_single_addr {
                0
            } else {
                bucket_info.buckets_remapping[orig_bucket]
            };

            let (flags, _second_bucket, mut extra_data, read) = read_info;

            let ind_read =
                CompressedReadIndipendent::from_read(&read, &mut buffers[bucket].reads_buffer);
            extra_data = F::AssociatedExtraData::copy_extra_from(
                extra_data,
                extra_buffer,
                &mut buffers[bucket].extra_buffer,
            );

            buckets_mults[orig_bucket] += 1;
            buffers[bucket].reads.push((flags, extra_data, ind_read));

            let packets_pool = &packets_pool;
            if buffers[bucket].reads.len() == buffers[bucket].reads.capacity() {
                replace_with_async(&mut buffers[bucket], |mut buffer| async move {
                    buffer.sub_bucket = bucket;
                    ops.packet_send(bucket_info.addresses[bucket].clone(), buffer);
                    track!(packets_pool.alloc_packet().await, PACKET_ALLOC_COUNTER)
                })
                .await;
            }
            F::AssociatedExtraData::clear_temp_buffer(extra_buffer);
        }

        for (bucket, (mut packet, address)) in buffers
            .drain(..)
            .zip(bucket_info.addresses.iter())
            .enumerate()
        {
            if packet.reads.len() > 0 {
                packet.sub_bucket = bucket;
                ops.packet_send(address.clone(), packet);
            }
        }
    }
}

impl<F: KmersTransformExecutorFactory> AsyncExecutor for KmersTransformReader<F> {
    type InputPacket = InputBucketDesc;
    type OutputPacket = ReadsBuffer<F::AssociatedExtraData>;
    type GlobalParams = KmersTransformContext<F>;
    type InitData = ();

    type AsyncExecutorFuture<'a> = impl Future<Output = ()> + 'a;

    fn new() -> Self {
        Self {
            _phantom: Default::default(),
        }
    }

    fn async_executor_main<'a>(
        &'a mut self,
        global_context: &'a KmersTransformContext<F>,
        mut receiver: ExecutorReceiver<Self>,
        _memory_tracker: MemoryTracker<Self>,
    ) -> Self::AsyncExecutorFuture<'a> {
        async move {
            let mut async_threads = Vec::new();

            while let Ok((address, _)) =
                track!(receiver.obtain_address().await, ADDR_WAITING_COUNTER)
            {
                let file = track!(
                    address.receive_packet().await.unwrap(),
                    PACKET_WAITING_COUNTER
                );
                let is_main_bucket = !file.resplitted && !file.rewritten;
                let is_resplitted = file.resplitted;
                let buckets_info = Self::compute_buckets(global_context, file);

                let reader_lock = global_context.reader_init_lock.lock().await;

                address.declare_addresses(
                    buckets_info.register_addresses.clone(),
                    PACKETS_PRIORITY_DEFAULT,
                );

                // FIXME: Better threads management
                while async_threads.len() < buckets_info.concurrency {
                    async_threads.push(AsyncReaderThread::new(DEFAULT_OUTPUT_BUFFER_SIZE / 2, 4));
                }

                let mut spawner = address.make_spawner();

                for ex_idx in 0..buckets_info.concurrency {
                    let async_thread = async_threads[ex_idx].clone();

                    let address = &address;
                    let buckets_info = &buckets_info;
                    let packets_pool = address.pool_alloc_await().await;

                    spawner.spawn_executor(async move {
                        Self::read_bucket(
                            global_context,
                            address,
                            buckets_info,
                            async_thread,
                            packets_pool,
                        )
                        .await;
                    });
                }

                drop(reader_lock);
                spawner.executors_await().await;

                if is_main_bucket {
                    global_context
                        .processed_buckets_count
                        .fetch_add(1, Ordering::Relaxed);
                    global_context
                        .processed_buckets_size
                        .fetch_add(buckets_info.file_size, Ordering::Relaxed);
                } else if is_resplitted {
                    global_context
                        .processed_extra_buckets_count
                        .fetch_add(1, Ordering::Relaxed);
                    global_context
                        .processed_extra_buckets_size
                        .fetch_add(buckets_info.file_size, Ordering::Relaxed);
                }

                assert!(track!(
                    address.receive_packet().await.is_none(),
                    PACKET_WAITING_COUNTER
                ));
            }
        }
    }
}

//
//     const MEMORY_FIELDS_COUNT: usize = 0;
//     const MEMORY_FIELDS: &'static [&'static str] = &[];
//
//     type BuildParams = (AsyncBinaryReader, usize, Vec<usize>, Vec<ExecutorAddress>);

//     fn allocate_new_group<E: ExecutorOperations<Self>>(
//         global_params: Arc<KmersTransformContext<F>>,
//         _memory_params: Option<Self::MemoryParams>,
//         packet: Option<Packet<InputBucketDesc>>,
//         mut ops: E,
//     ) -> (Self::BuildParams, usize) {
//     }
//
//     fn required_pool_items(&self) -> u64 {
//         1
//     }
//
//     fn pre_execute<E: ExecutorOperations<Self>>(
//         &mut self,
//         (reader, second_buckets_log_max, remappings, addresses): Self::BuildParams,
//         mut ops: E,
//     ) {
//     }
//
//     fn finalize<E: ExecutorOperations<Self>>(&mut self, _ops: E) {
//         assert_eq!(buffers.len(), 0);
//     }
