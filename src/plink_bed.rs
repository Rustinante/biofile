use math::{
    set::{
        ordered_integer_set::OrderedIntegerSet,
        traits::{Finite, Set},
    },
    stats::sum_f32,
    traits::ToIterator,
};
use ndarray::{Array, Axis, Ix2, ShapeBuilder};
use rayon::iter::{
    plumbing::{
        bridge, Consumer, Producer, ProducerCallback, UnindexedConsumer,
    },
    IndexedParallelIterator, IntoParallelIterator, ParallelIterator,
};
use std::{
    cmp::min,
    collections::HashSet,
    fs::{File, OpenOptions},
    io,
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
};

use plink_snps::PlinkSnps;

use crate::{byte_chunk_iter::ByteChunkIter, error::Error, util::get_buf};

pub const MAGIC_BYTES: [u8; 3] = [0x6c_u8, 0x1b_u8, 0x01_u8];
pub const NUM_MAGIC_BYTES: usize = 3;
const NUM_PEOPLE_PER_BYTE: usize = 4;

pub mod plink_snps;

pub struct PlinkBed {
    bed_path_list: Vec<String>,
    file_num_snps: Vec<(usize, PlinkSnpType)>,
    pub num_people: usize,
}

impl PlinkBed {
    /// `bfile_path_list` contains Vec<(bed, bim, fam)>
    pub fn new(
        bfile_path_list: &[(String, String, String, PlinkSnpType)],
    ) -> Result<PlinkBed, Error> {
        if bfile_path_list.is_empty() {
            return Err(Error::Generic(
                "the bfile_path_list has to contain at least one element"
                    .to_string(),
            ));
        }
        let bed_path_list: Vec<String> =
            bfile_path_list.iter().map(|t| t.0.to_string()).collect();

        for p in bed_path_list.iter() {
            PlinkBed::verify_magic_bytes(&p)?;
        }

        let file_num_snps: Vec<(usize, PlinkSnpType)> = bfile_path_list
            .iter()
            .map(|t| {
                let num_snps = get_line_count(&t.1)?;
                if num_snps == 0 {
                    Err(Error::Generic(
                        "cannot create PlinkBed with 0 SNPs".to_string(),
                    ))
                } else {
                    Ok((num_snps, t.3))
                }
            })
            .collect::<Result<Vec<(usize, PlinkSnpType)>, Error>>()?;

        let num_people: usize = {
            let num_people_set: HashSet<usize> = bfile_path_list
                .iter()
                .map(|t| Ok(get_line_count(&t.2)?))
                .collect::<Result<HashSet<usize>, Error>>()?;
            if num_people_set.len() > 1 {
                return Err(Error::Generic(
                    "inconsistent number of people across the bed files"
                        .to_string(),
                ));
            }
            let num_people =
                num_people_set.into_iter().collect::<Vec<usize>>()[0];
            if num_people == 0 {
                return Err(Error::Generic(
                    "cannot create PlinkBed with 0 people".to_string(),
                ));
            }
            num_people
        };

        println!("----------");
        bed_path_list
            .iter()
            .zip(file_num_snps.iter())
            .for_each(|(p, n)| {
                println!("{} num_snps: {}", p, n.0);
            });
        println!("num_people: {}\n----------", num_people,);

        Ok(PlinkBed {
            bed_path_list,
            file_num_snps,
            num_people,
        })
    }

    pub fn col_chunk_iter(
        &self,
        num_snps_per_iter: usize,
        range: Option<OrderedIntegerSet<usize>>,
    ) -> PlinkColChunkIter {
        match range {
            Some(range) => PlinkColChunkIter::new(
                self.file_num_snps.clone(),
                range,
                num_snps_per_iter,
                self.num_people,
                self.bed_path_list.clone(),
            ),
            None => PlinkColChunkIter::new(
                self.file_num_snps.clone(),
                OrderedIntegerSet::from_slice(&[[
                    0,
                    self.total_num_snps() - 1,
                ]]),
                num_snps_per_iter,
                self.num_people,
                self.bed_path_list.clone(),
            ),
        }
    }

    pub fn byte_chunk_iter(
        &self,
        file_index: usize,
        start_byte_index: usize,
        end_byte_index_exclusive: usize,
        chunk_size: usize,
    ) -> Result<ByteChunkIter<File>, Error> {
        match self.bed_path_list.get(file_index) {
            Some(p) => {
                let buf = match OpenOptions::new().read(true).open(p) {
                    Ok(file) => BufReader::new(file),
                    Err(io_error) => {
                        return Err(Error::IO {
                            why: format!("failed to open {}: {}", p, io_error),
                            io_error,
                        })
                    }
                };
                Ok(ByteChunkIter::new(
                    buf,
                    start_byte_index,
                    end_byte_index_exclusive,
                    chunk_size,
                ))
            }
            None => Err(Error::Generic(format!(
                "file index out of range {} >= {}",
                file_index,
                self.bed_path_list.len()
            ))),
        }
    }

    pub fn get_genotype_matrix(
        &self,
        snps_range: Option<OrderedIntegerSet<usize>>,
    ) -> Result<Array<f32, Ix2>, Error> {
        let num_snps = match &snps_range {
            None => self.total_num_snps(),
            Some(range) => range.size(),
        };
        let mut v = Vec::with_capacity(self.num_people * num_snps);

        for snp_chunk in self.col_chunk_iter(100, snps_range) {
            v.append(
                &mut snp_chunk.t().to_owned().as_slice().unwrap().to_vec(),
            );
        }
        let geno_arr = Array::from_shape_vec(
            (self.num_people, num_snps).strides((1, self.num_people)),
            v,
        )
        .unwrap();
        Ok(geno_arr)
    }

    pub fn get_bed_path_list(&self) -> &Vec<String> {
        &self.bed_path_list
    }

    pub fn get_file_num_snps(&self) -> &Vec<(usize, PlinkSnpType)> {
        &self.file_num_snps
    }

    pub fn total_num_snps(&self) -> usize {
        self.file_num_snps.iter().map(|pair| pair.0).sum::<usize>()
    }

    pub fn get_minor_allele_frequencies(
        &self,
        chunk_size: Option<usize>,
    ) -> Vec<f32> {
        let num_alleles = (self.num_people * 2) as f32;
        self.col_chunk_iter(chunk_size.unwrap_or(50), None)
            .into_par_iter()
            .flat_map(|snps| {
                snps.gencolumns()
                    .into_iter()
                    .map(|col| sum_f32(col.iter()) / num_alleles)
                    .collect::<Vec<f32>>()
            })
            .collect()
    }

    /// save the transpose of the BED file into `out_path`, which should have an
    /// extension of .bedt wherein the n-th sequence of bytes corresponds to
    /// the SNPs for the n-th person larger values of `snp_byte_chunk_size`
    /// lead to faster performance, at the cost of higher memory requirement
    pub fn create_bed_t(
        &mut self,
        file_index: usize,
        out_path: &str,
        snp_byte_chunk_size: usize,
    ) -> Result<(), Error> {
        let total_num_snps = self.total_num_snps();
        let num_bytes_per_snp = PlinkBed::num_bytes_per_snp(self.num_people);
        match self.bed_path_list.get(file_index) {
            Some(p) => {
                let mut bed_buf = get_buf(p)?;
                let mut buf_writer = BufWriter::new(
                    OpenOptions::new()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(out_path)?,
                );
                let num_bytes_per_person = usize_div_ceil(total_num_snps, 4);

                let people_stride = snp_byte_chunk_size * 4;
                let mut snp_bytes = vec![0u8; snp_byte_chunk_size];

                // write people_stride people at a time
                for j in (0..self.num_people).step_by(people_stride) {
                    let mut people_buf =
                        vec![vec![0u8; num_bytes_per_person]; people_stride];
                    if self.num_people - j < people_stride {
                        let remaining_people = self.num_people % people_stride;
                        snp_bytes =
                            vec![0u8; usize_div_ceil(remaining_people, 4)];
                    }
                    let relative_seek_offset =
                        (num_bytes_per_snp - snp_bytes.len()) as i64;
                    // read 4 SNPs to the buffers at a time
                    PlinkBed::seek_to_byte_containing_snp_i_person_j(
                        &mut bed_buf,
                        0,
                        j,
                        num_bytes_per_snp,
                    )?;
                    for (snp_byte_index, k) in
                        (0..total_num_snps).step_by(4).enumerate()
                    {
                        for (snp_offset, _) in
                            (k..min(k + 4, total_num_snps)).enumerate()
                        {
                            bed_buf.read_exact(&mut snp_bytes)?;
                            for w in 0..snp_bytes.len() {
                                people_buf[w][snp_byte_index] |=
                                    (snp_bytes[w] & 0b11) << (snp_offset << 1);
                                people_buf[w + 1][snp_byte_index] |=
                                    ((snp_bytes[w] >> 2) & 0b11)
                                        << (snp_offset << 1);
                                people_buf[w + 2][snp_byte_index] |=
                                    ((snp_bytes[w] >> 4) & 0b11)
                                        << (snp_offset << 1);
                                people_buf[w + 3][snp_byte_index] |=
                                    ((snp_bytes[w] >> 6) & 0b11)
                                        << (snp_offset << 1);
                            }
                            bed_buf.seek_relative(relative_seek_offset)?;
                        }
                    }
                    for (p, buf) in people_buf.iter().enumerate() {
                        if j + p < self.num_people {
                            buf_writer.write_all(buf.as_slice())?;
                        }
                    }
                }
                Ok(())
            }
            None => Err(Error::Generic(format!(
                "file index out of range {} >= {}",
                file_index,
                self.bed_path_list.len()
            ))),
        }
    }

    pub fn create_dominance_geno_bed(
        &self,
        file_index: usize,
        out_path: &str,
    ) -> Result<(), Error> {
        let num_bytes_per_snp = PlinkBed::num_bytes_per_snp(self.num_people);
        let mut writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(out_path)?,
        );
        writer.write_all(&PlinkBed::get_magic_bytes())?;
        for bytes in self.byte_chunk_iter(
            file_index,
            NUM_MAGIC_BYTES,
            NUM_MAGIC_BYTES + self.total_num_snps() * num_bytes_per_snp,
            num_bytes_per_snp,
        )? {
            let out_bytes = PlinkSnps::from_geno(
                PlinkSnps::new(bytes, self.num_people)
                    .into_iter()
                    .map(|s| match s {
                        2 => 1,
                        s => s,
                    })
                    .collect(),
            )
            .into_bytes();
            writer.write_all(&out_bytes)?;
        }
        Ok(())
    }

    // the first person is the lowest two bits
    // 00 -> 2 homozygous for the first allele in the .bim file (usually the
    // minor allele) 01 -> 0 missing genotype
    // 10 -> 1 heterozygous
    // 11 -> 0 homozygous for the second allele in the .bim file (usually the
    // major allele)
    pub fn create_bed(
        arr: &Array<u8, Ix2>,
        out_path: &str,
    ) -> Result<(), Error> {
        let (num_people, _num_snps) = arr.dim();
        let mut buf_writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(out_path)?,
        );
        buf_writer.write_all(&[0x6c, 0x1b, 0x1])?;
        for col in arr.gencolumns() {
            let mut i = 0;
            for _ in 0..num_people / 4 {
                buf_writer.write_all(&[geno_to_lowest_two_bits(col[i])
                    | (geno_to_lowest_two_bits(col[i + 1]) << 2)
                    | (geno_to_lowest_two_bits(col[i + 2]) << 4)
                    | (geno_to_lowest_two_bits(col[i + 3]) << 6)])?;
                i += 4;
            }
            let remainder = num_people % 4;
            if remainder > 0 {
                let mut byte = 0u8;
                for j in 0..remainder {
                    byte |= geno_to_lowest_two_bits(col[i + j]) << (j * 2);
                }
                buf_writer.write_all(&[byte])?;
            }
        }
        Ok(())
    }

    fn verify_magic_bytes(bed_filepath: &str) -> Result<(), Error> {
        let mut bed_buf = get_buf(bed_filepath)?;

        // check if PLINK bed file has the correct file signature
        let mut magic_bytes = [0u8; 3];
        if let Err(io_error) = bed_buf.read_exact(&mut magic_bytes) {
            return Err(Error::IO {
                why: format!(
                    "Failed to read the first three bytes of {}: {}",
                    bed_filepath, io_error
                ),
                io_error,
            });
        }
        let expected_bytes = PlinkBed::get_magic_bytes();
        if magic_bytes != expected_bytes {
            return Err(Error::BadFormat(format!(
                "The first three bytes of the PLINK bed file {} are supposed to be 0x{:x?}, but found 0x{:x?}",
                bed_filepath, expected_bytes, magic_bytes
            )));
        }
        Ok(())
    }

    #[inline]
    pub fn get_magic_bytes() -> [u8; 3] {
        MAGIC_BYTES
    }

    #[inline]
    pub fn get_num_magic_bytes() -> usize {
        NUM_MAGIC_BYTES
    }

    #[inline]
    fn num_bytes_per_snp(num_people: usize) -> usize {
        usize_div_ceil(num_people, NUM_PEOPLE_PER_BYTE)
    }

    /// makes the BufReader point to the start of the byte containing the SNP i
    /// individual j 0-indexing
    fn seek_to_byte_containing_snp_i_person_j<B: Seek>(
        buf: &mut B,
        snp_i: usize,
        person_j: usize,
        num_bytes_per_snp: usize,
    ) -> Result<(), io::Error> {
        // the first NUM_MAGIC_BYTES bytes are the file signature
        buf.seek(SeekFrom::Start(
            (NUM_MAGIC_BYTES
                + num_bytes_per_snp * snp_i
                + person_j / NUM_PEOPLE_PER_BYTE) as u64,
        ))?;
        Ok(())
    }
}

fn usize_div_ceil(a: usize, divisor: usize) -> usize {
    a / divisor + (a % divisor != 0) as usize
}

pub fn lowest_two_bits_to_geno(byte: u8) -> u8 {
    // 00 -> 2 homozygous for the first allele in the .bim file (usually the
    // minor allele) 01 -> 0 missing genotype
    // 10 -> 1 heterozygous
    // 11 -> 0 homozygous for the second allele in the .bim file (usually the
    // major allele)
    let a = (byte & 0b10) >> 1;
    let b = byte & 1;
    (((a | b) ^ 1) << 1) | (a & (!b))
}

pub fn geno_to_lowest_two_bits(geno: u8) -> u8 {
    // 00 -> 2 homozygous for the first allele in the .bim file (usually the
    // minor allele) 01 -> 0 missing genotype
    // 10 -> 1 heterozygous
    // 11 -> 0 homozygous for the second allele in the .bim file (usually the
    // major allele)
    let not_a = ((geno & 0b10) >> 1) ^ 1;
    let not_b = (geno & 1) ^ 1;
    (not_a << 1) | (not_b & not_a)
}

fn get_num_people_last_byte(total_num_people: usize) -> Option<usize> {
    if total_num_people == 0 {
        None
    } else {
        match total_num_people % NUM_PEOPLE_PER_BYTE {
            0 => Some(NUM_PEOPLE_PER_BYTE),
            x => Some(x),
        }
    }
}

fn get_line_count(filename: &str) -> Result<usize, Error> {
    let fam_buf = get_buf(filename)?;
    Ok(fam_buf.lines().count())
}

struct FileSnpIndexer {
    file_num_snps: Vec<(usize, PlinkSnpType)>,
}

impl FileSnpIndexer {
    fn new(file_num_snps: Vec<(usize, PlinkSnpType)>) -> FileSnpIndexer {
        FileSnpIndexer {
            file_num_snps,
        }
    }

    /// returns a `Some` of a tuple (file_index, snp_index_within_file)
    /// if the SNP is within range. `None` otherwise.
    fn get_file_snp_index(
        &self,
        snp_index: usize,
    ) -> Option<(usize, usize, PlinkSnpType)> {
        let mut acc = 0;
        for (file_index, (count, snp_type)) in
            self.file_num_snps.iter().enumerate()
        {
            if snp_index < acc + *count {
                return Some((file_index, snp_index - acc, *snp_type));
            }
            acc += *count;
        }
        None
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum PlinkSnpType {
    Additive,
    Dominance,
}

pub struct PlinkColChunkIter {
    buf: Vec<BufReader<File>>,
    file_num_snps: Vec<(usize, PlinkSnpType)>,
    range: OrderedIntegerSet<usize>,
    num_snps_per_iter: usize,
    num_people: usize,
    num_snps_in_range: usize,
    range_cursor: usize,
    last_read_file_snp_index: Option<(usize, usize)>,
    bed_path_list: Vec<String>,
    file_snp_indexer: FileSnpIndexer,
}

impl PlinkColChunkIter {
    pub fn new(
        file_num_snps: Vec<(usize, PlinkSnpType)>,
        range: OrderedIntegerSet<usize>,
        num_snps_per_iter: usize,
        num_people: usize,
        bed_path_list: Vec<String>,
    ) -> PlinkColChunkIter {
        let num_snps_in_range = range.size();
        let first = range.first();
        let buf = PlinkColChunkIter::get_buf_list(&bed_path_list).unwrap();
        let file_snp_indexer = FileSnpIndexer::new(file_num_snps.clone());
        let mut iter = PlinkColChunkIter {
            buf,
            file_num_snps,
            range,
            num_snps_per_iter,
            num_people,
            num_snps_in_range,
            range_cursor: 0,
            last_read_file_snp_index: None,
            bed_path_list,
            file_snp_indexer,
        };
        if let Some(start) = first {
            iter.seek_to_snp(start).unwrap();
        } else {
            iter.seek_to_snp(0).unwrap();
        }
        iter
    }

    fn get_buf_list(
        bed_path_list: &[String],
    ) -> Result<Vec<BufReader<File>>, Error> {
        Ok(bed_path_list
            .iter()
            .map(|p| Ok(get_buf(p)?))
            .collect::<Result<Vec<BufReader<File>>, Error>>()?)
    }

    fn seek_to_snp(&mut self, snp_index: usize) -> Result<(), Error> {
        if !self.range.contains(&snp_index) {
            return Err(Error::Generic(format!(
                "SNP index {} is not in the iterator range",
                snp_index
            )));
        }
        let num_bytes_per_snp = PlinkBed::num_bytes_per_snp(self.num_people);
        match self.file_snp_indexer.get_file_snp_index(snp_index) {
            Some((file_index, snp_index_within_file, _snp_type)) => {
                // skip the first NUM_MAGIC_BYTES magic bytes
                self.buf[file_index].seek(SeekFrom::Start(
                    NUM_MAGIC_BYTES as u64
                        + (num_bytes_per_snp * snp_index_within_file) as u64,
                ))?;
                Ok(())
            }
            None => Err(Error::Generic(format!(
                "failed to get file snp index for snp_index {}",
                snp_index
            ))),
        }
    }

    fn read_snp_bytes(
        &mut self,
        snp_index: usize,
        mut snp_bytes_buf: &mut Vec<u8>,
    ) -> Result<PlinkSnpType, Error> {
        let num_bytes_per_snp = PlinkBed::num_bytes_per_snp(self.num_people);
        match self.file_snp_indexer.get_file_snp_index(snp_index) {
            Some((file_index, snp_index_within_file, snp_type)) => {
                if let Some((last_file_index, last_snp_index_within_file)) =
                    self.last_read_file_snp_index
                {
                    if file_index == last_file_index {
                        let snp_index_gap =
                            snp_index_within_file - last_snp_index_within_file;
                        if snp_index_gap > 1 {
                            self.buf[file_index]
                                .seek_relative(
                                    ((snp_index_gap - 1) * num_bytes_per_snp)
                                        as i64,
                                )
                                .unwrap();
                        }
                        self.buf[file_index].read_exact(&mut snp_bytes_buf)?;
                        self.last_read_file_snp_index =
                            Some((file_index, snp_index_within_file));
                        return Ok(snp_type);
                    }
                }
                self.seek_to_snp(snp_index)?;
                self.buf[file_index].read_exact(&mut snp_bytes_buf)?;
                self.last_read_file_snp_index =
                    Some((file_index, snp_index_within_file));
                Ok(snp_type)
            }
            None => Err(Error::Generic(format!(
                "SNP index {} out of range",
                snp_index
            ))),
        }
    }

    /// indices are 0 based
    #[inline]
    fn clone_with_range(
        &self,
        range: OrderedIntegerSet<usize>,
    ) -> PlinkColChunkIter {
        PlinkColChunkIter::new(
            self.file_num_snps.clone(),
            range,
            self.num_snps_per_iter,
            self.num_people,
            self.bed_path_list.clone(),
        )
    }

    fn read_chunk(&mut self, chunk_size: usize) -> Array<f32, Ix2> {
        let num_bytes_per_snp = PlinkBed::num_bytes_per_snp(self.num_people);
        let num_people_last_byte =
            get_num_people_last_byte(self.num_people).unwrap_or(0);

        let snp_indices = self
            .range
            .slice(self.range_cursor..self.range_cursor + chunk_size);
        let actual_chunk_size = snp_indices.size();
        self.range_cursor += actual_chunk_size;

        let mut v = Vec::with_capacity(self.num_people * actual_chunk_size);
        let mut snp_bytes = vec![0u8; num_bytes_per_snp];
        for index in snp_indices.to_iter() {
            let snp_type = self.read_snp_bytes(index, &mut snp_bytes).unwrap();
            let mut snp_vec = Vec::with_capacity(self.num_people);
            for i in 0..num_bytes_per_snp - 1 {
                snp_vec.push(lowest_two_bits_to_geno(snp_bytes[i]) as f32);
                snp_vec.push(lowest_two_bits_to_geno(snp_bytes[i] >> 2) as f32);
                snp_vec.push(lowest_two_bits_to_geno(snp_bytes[i] >> 4) as f32);
                snp_vec.push(lowest_two_bits_to_geno(snp_bytes[i] >> 6) as f32);
            }
            // last byte
            for k in 0..num_people_last_byte {
                snp_vec.push(lowest_two_bits_to_geno(
                    snp_bytes[num_bytes_per_snp - 1] >> (k << 1),
                ) as f32);
            }
            v.append(&mut match snp_type {
                PlinkSnpType::Additive => snp_vec,
                PlinkSnpType::Dominance => {
                    convert_geno_vec_to_dominance_representation(snp_vec)
                }
            });
        }
        Array::from_shape_vec(
            (self.num_people, actual_chunk_size).strides((1, self.num_people)),
            v,
        )
        .unwrap()
    }
}

fn convert_geno_vec_to_dominance_representation(
    mut geno_vec: Vec<f32>,
) -> Vec<f32> {
    let num_people = geno_vec.len();
    let double_num_people = (2 * num_people) as f32;
    let p = sum_f32(geno_vec.iter()) / double_num_people;
    let hetero = 2. * p;
    let homo_minor = 4. * p - 2.;
    for i in 0..num_people {
        geno_vec[i] = match geno_vec[i] as u8 {
            2 => homo_minor,
            1 => hetero,
            _ => 0.,
        };
    }
    geno_vec
}

pub fn convert_geno_arr_to_dominance_representation(
    mut geno_arr: Array<f32, Ix2>,
) -> Array<f32, Ix2> {
    let num_people = geno_arr.dim().0;
    let double_num_people = (2 * num_people) as f32;
    for mut col in geno_arr.axis_iter_mut(Axis(1)) {
        let p = sum_f32(col.iter()) / double_num_people;
        let hetero = 2. * p;
        let homo_minor = 4. * p - 2.;
        for i in 0..num_people {
            col[i] = match col[i] as u8 {
                2 => homo_minor,
                1 => hetero,
                _ => 0.,
            };
        }
    }
    geno_arr
}

impl IntoParallelIterator for PlinkColChunkIter {
    type Item = <PlinkColChunkParallelIter as ParallelIterator>::Item;
    type Iter = PlinkColChunkParallelIter;

    fn into_par_iter(self) -> Self::Iter {
        PlinkColChunkParallelIter {
            iter: self,
        }
    }
}

impl Iterator for PlinkColChunkIter {
    type Item = Array<f32, Ix2>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range_cursor >= self.num_snps_in_range {
            return None;
        }
        let chunk_size = min(
            self.num_snps_per_iter,
            self.num_snps_in_range - self.range_cursor,
        );
        Some(self.read_chunk(chunk_size))
    }
}

impl ExactSizeIterator for PlinkColChunkIter {
    fn len(&self) -> usize {
        usize_div_ceil(
            self.num_snps_in_range - self.range_cursor,
            self.num_snps_per_iter,
        )
    }
}

impl DoubleEndedIterator for PlinkColChunkIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.range_cursor >= self.num_snps_in_range {
            return None;
        }
        let chunk_size = min(
            self.num_snps_per_iter,
            self.num_snps_in_range - self.range_cursor,
        );
        // reading from the back is equivalent to reducing the number of SNPs in
        // range
        self.num_snps_in_range -= chunk_size;

        // save and restore self.last_read_snp_index after the call to
        // self.read_chunk we set the self.last_read_snp_index to None
        // to prevent self.read_chunk from performing seek_relative on
        // the buffer
        let last_read_snp_index = self.last_read_file_snp_index;
        self.last_read_file_snp_index = None;

        let snp = self
            .range
            .slice(self.num_snps_in_range..self.num_snps_in_range + 1)
            .first()
            .unwrap();
        self.seek_to_snp(snp).unwrap();
        let chunk = self.read_chunk(chunk_size);
        match last_read_snp_index {
            Some((file_i, snp_i)) => {
                let snp_index = self
                    .file_num_snps
                    .iter()
                    .take(file_i)
                    .map(|pair| pair.0)
                    .sum::<usize>()
                    + snp_i;
                self.seek_to_snp(snp_index).unwrap();
            }
            None => self.seek_to_snp(0).unwrap(),
        };
        self.last_read_file_snp_index = last_read_snp_index;
        Some(chunk)
    }
}

struct ColChunkIterProducer {
    iter: PlinkColChunkIter,
}

impl Producer for ColChunkIterProducer {
    type IntoIter = PlinkColChunkIter;
    type Item = <PlinkColChunkIter as Iterator>::Item;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter
    }

    fn split_at(self, index: usize) -> (Self, Self) {
        let mid_range_index =
            min(self.iter.num_snps_per_iter * index, self.iter.range.size());
        (
            ColChunkIterProducer {
                iter: self.iter.clone_with_range(
                    self.iter.range.slice(0..mid_range_index),
                ),
            },
            ColChunkIterProducer {
                iter: self.iter.clone_with_range(
                    self.iter
                        .range
                        .slice(mid_range_index..self.iter.range.size()),
                ),
            },
        )
    }
}

impl IntoIterator for ColChunkIterProducer {
    type IntoIter = PlinkColChunkIter;
    type Item = <PlinkColChunkIter as Iterator>::Item;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter
    }
}

pub struct PlinkColChunkParallelIter {
    iter: PlinkColChunkIter,
}

impl ParallelIterator for PlinkColChunkParallelIter {
    type Item = <PlinkColChunkIter as Iterator>::Item;

    fn drive_unindexed<C>(self, consumer: C) -> C::Result
    where
        C: UnindexedConsumer<Self::Item>, {
        bridge(self, consumer)
    }

    fn opt_len(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

impl IndexedParallelIterator for PlinkColChunkParallelIter {
    fn len(&self) -> usize {
        self.iter.len()
    }

    fn drive<C>(self, consumer: C) -> C::Result
    where
        C: Consumer<Self::Item>, {
        bridge(self, consumer)
    }

    fn with_producer<CB>(self, callback: CB) -> CB::Output
    where
        CB: ProducerCallback<Self::Item>, {
        callback.callback(ColChunkIterProducer {
            iter: self.iter,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cmp::min, io, io::Write};

    use math::{
        set::ordered_integer_set::OrderedIntegerSet, traits::ToIterator,
    };
    use ndarray::{array, s, stack, Array, Axis, Ix2};
    use ndarray_rand::RandomExt;
    use rand::distributions::Uniform;
    use tempfile::{NamedTempFile, TempPath};

    use crate::plink_bed::{
        convert_geno_arr_to_dominance_representation, PlinkBed, PlinkSnpType,
    };

    fn create_dummy_bim_fam(
        mut bim: &mut NamedTempFile,
        mut fam: &mut NamedTempFile,
        num_people: usize,
        num_snps: usize,
    ) -> Result<(), io::Error> {
        write_dummy_bim(&mut bim, num_snps)?;
        write_dummy_fam(&mut fam, num_people)?;
        Ok(())
    }

    fn write_dummy_bim(
        bim: &mut NamedTempFile,
        num_snps: usize,
    ) -> Result<(), io::Error> {
        for i in 1..=num_snps {
            bim.write_fmt(format_args!("{}\n", i))?;
        }
        Ok(())
    }

    fn write_dummy_fam(
        fam: &mut NamedTempFile,
        num_people: usize,
    ) -> Result<(), io::Error> {
        for i in 1..=num_people {
            fam.write_fmt(format_args!("{}\n", i))?;
        }
        Ok(())
    }

    #[test]
    fn test_create_bed() {
        fn test(geno: &Array<u8, Ix2>) {
            let mut bim = NamedTempFile::new().unwrap();
            let mut fam = NamedTempFile::new().unwrap();
            create_dummy_bim_fam(
                &mut bim,
                &mut fam,
                geno.dim().0,
                geno.dim().1,
            )
            .unwrap();
            let path = NamedTempFile::new().unwrap().into_temp_path();
            let path_str = path.to_str().unwrap().to_string();
            PlinkBed::create_bed(&geno, &path_str).unwrap();
            let geno_bed = PlinkBed::new(&[(
                path_str,
                bim.into_temp_path().to_str().unwrap().to_string(),
                fam.into_temp_path().to_str().unwrap().to_string(),
                PlinkSnpType::Additive,
            )])
            .unwrap();
            assert_eq!(
                geno.mapv(|x| x as f32),
                geno_bed.get_genotype_matrix(None).unwrap()
            );
        }
        test(&array![[0],]);
        test(&array![[1],]);
        test(&array![[2],]);
        test(&array![[0, 1, 2],]);
        test(&array![[0], [1], [2],]);
        test(&array![[0, 0, 1], [1, 1, 2], [0, 2, 1],]);
        test(&array![
            [0, 0, 1, 2],
            [1, 1, 2, 1],
            [2, 0, 0, 0],
            [1, 0, 0, 2],
            [0, 2, 1, 0],
        ]);
        test(&array![
            [0, 0, 1, 2, 1],
            [1, 1, 2, 1, 2],
            [2, 0, 0, 0, 0],
            [1, 0, 0, 2, 2],
            [0, 2, 1, 0, 1],
        ]);
        test(&array![
            [0, 0, 1, 2, 1],
            [1, 0, 0, 2, 1],
            [2, 0, 2, 0, 0],
            [1, 1, 0, 2, 2],
            [0, 2, 2, 1, 1],
            [2, 1, 2, 0, 0],
            [1, 2, 0, 1, 2],
            [2, 0, 1, 0, 1],
        ]);
        test(&array![
            [0, 0, 1, 2, 1, 2, 2, 0],
            [1, 0, 0, 2, 1, 2, 1, 1],
            [2, 0, 2, 0, 0, 0, 2, 1],
            [1, 1, 0, 2, 2, 1, 1, 1],
            [0, 2, 2, 1, 1, 2, 0, 2],
            [2, 1, 2, 0, 0, 0, 2, 2],
            [1, 2, 0, 1, 2, 1, 1, 0],
            [2, 0, 1, 0, 1, 0, 0, 2],
        ]);
    }

    #[test]
    fn test_multiple_bfiles() {
        let (num_people, num_snps_1, num_snps_2) = (137usize, 71usize, 37usize);
        let geno_1 =
            Array::random((num_people, num_snps_1), Uniform::from(0..3));
        let geno_2 =
            Array::random((num_people, num_snps_2), Uniform::from(0..3));
        let mut bim_1 = NamedTempFile::new().unwrap();
        let mut bim_2 = NamedTempFile::new().unwrap();
        let mut fam = NamedTempFile::new().unwrap();
        write_dummy_fam(&mut fam, num_people).unwrap();
        write_dummy_bim(&mut bim_1, num_snps_1).unwrap();
        write_dummy_bim(&mut bim_2, num_snps_2).unwrap();
        let bed_path_1 = NamedTempFile::new().unwrap().into_temp_path();
        let bed_path_2 = NamedTempFile::new().unwrap().into_temp_path();
        let bim_path_1 = bim_1.into_temp_path();
        let bim_path_2 = bim_2.into_temp_path();
        let fam_path = fam.into_temp_path();
        PlinkBed::create_bed(&geno_1, bed_path_1.to_str().unwrap()).unwrap();
        PlinkBed::create_bed(&geno_2, bed_path_2.to_str().unwrap()).unwrap();

        let bed = PlinkBed::new(&[
            (
                bed_path_1.to_str().unwrap().to_string(),
                bim_path_1.to_str().unwrap().to_string(),
                fam_path.to_str().unwrap().to_string(),
                PlinkSnpType::Additive,
            ),
            (
                bed_path_2.to_str().unwrap().to_string(),
                bim_path_2.to_str().unwrap().to_string(),
                fam_path.to_str().unwrap().to_string(),
                PlinkSnpType::Additive,
            ),
        ])
        .unwrap();
        let true_geno_arr = stack![Axis(1), geno_1, geno_2].mapv(|x| x as f32);
        assert_eq!(true_geno_arr, bed.get_genotype_matrix(None).unwrap());
    }

    #[test]
    fn test_chunk_iter() {
        let (num_people, num_snps) = (137usize, 71usize);
        let geno = Array::random((num_people, num_snps), Uniform::from(0..3));

        let mut bim = NamedTempFile::new().unwrap();
        let mut fam = NamedTempFile::new().unwrap();
        create_dummy_bim_fam(&mut bim, &mut fam, num_people, num_snps).unwrap();
        let bed_file = NamedTempFile::new().unwrap();
        let bed_path = bed_file.into_temp_path();
        let bed_path_str = bed_path.to_str().unwrap().to_string();
        PlinkBed::create_bed(&geno, &bed_path_str).unwrap();

        let bed = PlinkBed::new(&[(
            bed_path_str,
            bim.into_temp_path().to_str().unwrap().to_string(),
            fam.into_temp_path().to_str().unwrap().to_string(),
            PlinkSnpType::Additive,
        )])
        .unwrap();
        let true_geno_arr = geno.mapv(|x| x as f32);

        // test get_genotype_matrix
        assert_eq!(bed.get_genotype_matrix(None).unwrap(), true_geno_arr);

        let chunk_size = 5;
        for (i, snps) in bed.col_chunk_iter(chunk_size, None).enumerate() {
            let end_index = min((i + 1) * chunk_size, true_geno_arr.dim().1);
            assert!(
                true_geno_arr.slice(s![.., i * chunk_size..end_index]) == snps
            );
        }

        let snp_index_slices =
            OrderedIntegerSet::from_slice(&[[2, 4], [6, 9], [20, 46], [
                70, 70,
            ]]);
        for (i, snps) in bed
            .col_chunk_iter(chunk_size, Some(snp_index_slices.clone()))
            .enumerate()
        {
            let end_index = min((i + 1) * chunk_size, true_geno_arr.dim().1);
            let snp_indices = snp_index_slices.slice(i * chunk_size..end_index);
            for (k, j) in snp_indices.to_iter().enumerate() {
                assert_eq!(
                    true_geno_arr.slice(s![.., j]),
                    snps.slice(s![.., k])
                );
            }
        }

        // test get_genotype_matrix
        let geno = bed
            .get_genotype_matrix(Some(snp_index_slices.clone()))
            .unwrap();
        let mut arr = Array::zeros((num_people, 35));
        for (jj, j) in snp_index_slices.to_iter().enumerate() {
            for i in 0..num_people {
                arr[[i, jj]] = true_geno_arr[[i, j]];
            }
        }
        assert_eq!(arr, geno);
    }

    fn create_temp_geno_bfile(
        geno: &Array<u8, Ix2>,
    ) -> (TempPath, TempPath, TempPath) {
        let mut bim = NamedTempFile::new().unwrap();
        let mut fam = NamedTempFile::new().unwrap();
        create_dummy_bim_fam(&mut bim, &mut fam, geno.dim().0, geno.dim().1)
            .unwrap();
        let bed_path = NamedTempFile::new().unwrap().into_temp_path();
        let bed_path_str = bed_path.to_str().unwrap().to_string();
        PlinkBed::create_bed(&geno, &bed_path_str).unwrap();
        let bim_path = bim.into_temp_path();
        let fam_path = fam.into_temp_path();
        (bed_path, bim_path, fam_path)
    }

    fn assert_arr_almost_eq_f32(
        arr1: &Array<f32, Ix2>,
        arr2: &Array<f32, Ix2>,
        eps: f32,
    ) {
        let (num_rows, num_cols) = arr1.dim();
        assert_eq!((num_rows, num_cols), arr2.dim());
        for i in 0..num_rows {
            for j in 0..num_cols {
                assert!(
                    (arr1[[i, j]] - arr2[[i, j]]).abs() < eps,
                    "arr1[{}, {}]: {} arr2[{}, {}]: {} ",
                    i,
                    j,
                    arr1[[i, j]],
                    i,
                    j,
                    arr2[[i, j]]
                );
            }
        }
    }

    #[test]
    fn test_create_dominance_geno_bed() {
        fn test(geno: &Array<u8, Ix2>) {
            let (bed_path, bim_path, fam_path) = create_temp_geno_bfile(geno);
            let geno_bed = PlinkBed::new(&[(
                bed_path.to_str().unwrap().to_string(),
                bim_path.to_str().unwrap().to_string(),
                fam_path.to_str().unwrap().to_string(),
                PlinkSnpType::Additive,
            )])
            .unwrap();
            let dominance_path = NamedTempFile::new().unwrap().into_temp_path();
            geno_bed
                .create_dominance_geno_bed(0, dominance_path.to_str().unwrap())
                .unwrap();
            let dominance_geno = PlinkBed::new(&[(
                dominance_path.to_str().unwrap().to_string(),
                bim_path.to_str().unwrap().to_string(),
                fam_path.to_str().unwrap().to_string(),
                PlinkSnpType::Additive,
            )])
            .unwrap();
            assert_eq!(
                geno_bed.get_genotype_matrix(None).unwrap().mapv(|s| {
                    match s as u8 {
                        2 => 1u8,
                        s => s,
                    }
                }),
                dominance_geno
                    .get_genotype_matrix(None)
                    .unwrap()
                    .mapv(|s| s as u8)
            );
        }
        test(&array![
            [0, 0, 1, 2],
            [1, 1, 2, 1],
            [2, 0, 0, 0],
            [1, 0, 0, 2],
            [0, 2, 1, 0],
        ]);

        test(&array![
            [0, 0, 1, 2, 2],
            [1, 1, 2, 1, 0],
            [2, 0, 0, 0, 2],
            [1, 0, 0, 2, 1],
            [0, 2, 1, 0, 1],
        ]);

        test(&array![
            [0, 0, 1, 2, 2],
            [1, 1, 2, 1, 0],
            [2, 0, 0, 0, 2],
            [1, 0, 0, 2, 1],
            [0, 1, 2, 1, 2],
            [2, 1, 2, 0, 1],
            [1, 0, 1, 1, 0],
            [2, 1, 0, 2, 0],
        ]);

        test(&array![
            [0, 0, 1, 2, 2, 1, 1, 0],
            [1, 1, 2, 1, 0, 0, 0, 0],
            [2, 0, 0, 0, 2, 1, 0, 1],
            [1, 0, 0, 2, 1, 1, 2, 0],
            [0, 1, 2, 1, 2, 1, 1, 2],
            [2, 1, 2, 0, 1, 0, 2, 0],
            [1, 0, 1, 1, 0, 0, 0, 2],
            [2, 1, 0, 2, 0, 0, 1, 1],
        ]);

        test(&array![
            [0, 0, 1, 2, 2, 1, 1, 0, 2],
            [1, 1, 2, 1, 0, 0, 0, 0, 1],
            [2, 0, 0, 0, 2, 1, 0, 1, 1],
            [1, 0, 0, 2, 1, 1, 2, 0, 2],
            [0, 1, 2, 1, 2, 1, 1, 2, 2],
            [2, 1, 2, 0, 1, 0, 2, 0, 0],
            [1, 0, 1, 1, 0, 0, 0, 2, 0],
            [2, 1, 0, 2, 0, 0, 1, 1, 2],
        ]);
    }

    #[test]
    fn test_convert_to_dominance_representation() {
        fn test(standard_snp_arr: Array<u8, Ix2>, expected: Array<f32, Ix2>) {
            let (bed_path, bim_path, fam_path) =
                create_temp_geno_bfile(&standard_snp_arr);

            let eps = 1e-6;
            let actual = convert_geno_arr_to_dominance_representation(
                standard_snp_arr.mapv(|x| x as f32),
            );
            assert_arr_almost_eq_f32(&actual, &expected, eps);

            let geno_bed = PlinkBed::new(&[(
                bed_path.to_str().unwrap().to_string(),
                bim_path.to_str().unwrap().to_string(),
                fam_path.to_str().unwrap().to_string(),
                PlinkSnpType::Dominance,
            )])
            .unwrap();
            let dominance_snps = geno_bed.get_genotype_matrix(None).unwrap();
            assert_arr_almost_eq_f32(&dominance_snps, &expected, eps)
        }

        test(
            array![
                [0, 1, 2, 2, 2, 0],
                [2, 2, 0, 1, 2, 0],
                [1, 1, 2, 1, 0, 0],
                [1, 0, 1, 2, 1, 2],
                [0, 2, 2, 1, 1, 1],
            ],
            array![
                [0., 1.2, 0.8, 0.8, 0.4, 0.],
                [-0.4, 0.4, 0., 1.4, 0.4, 0.],
                [0.8, 1.2, 0.8, 1.4, 0., 0.],
                [0.8, 0., 1.4, 0.8, 1.2, -0.8],
                [0., 0.4, 0.8, 1.4, 1.2, 0.6],
            ],
        );
    }
}
