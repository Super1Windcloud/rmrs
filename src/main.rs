use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rayon::prelude::*;

#[derive(Debug, Default)]
struct Stats {
    files_deleted: AtomicU64,
    dirs_deleted: AtomicU64,
    bytes_deleted: AtomicU64,
    errors: AtomicU64,
}

impl Stats {
    fn increment_files(&self) {
        self.files_deleted.fetch_add(1, Ordering::Relaxed);
    }
    fn increment_dirs(&self) {
        self.dirs_deleted.fetch_add(1, Ordering::Relaxed);
    }
    fn add_bytes(&self, bytes: u64) {
        self.bytes_deleted.fetch_add(bytes, Ordering::Relaxed);
    }
    fn increment_errors(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn print_summary(&self) {
        println!("删除完成:");
        println!("  文件: {}", self.files_deleted.load(Ordering::Relaxed));
        println!("  目录: {}", self.dirs_deleted.load(Ordering::Relaxed));
        println!(
            "  大小: {:.2} MB",
            self.bytes_deleted.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0
        );
        println!("  错误: {}", self.errors.load(Ordering::Relaxed));
    }
}

/// 简化方案1: 批量并发处理顶层目录
fn parallel_remove_top_level(paths: &[PathBuf], num_threads: usize) -> io::Result<()> {
    let stats = Arc::new(Stats::default());

    // 如果路径数量少，直接并发处理每个顶层路径
    if paths.len() >= num_threads {
        paths.par_iter().for_each(|path| {
            if let Err(e) = remove_path_recursive(path, &stats) {
                eprintln!("删除失败 '{}': {}", path.display(), e);
                stats.increment_errors();
            }
        });
    } else {
        // 路径数量少时，展开第一层目录进行并发
        let mut all_items = Vec::new();

        for path in paths {
            if path.is_dir() {
                match fs::read_dir(path) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            all_items.push(entry.path());
                        }
                    }
                    Err(_) => {
                        all_items.push(path.clone());
                    }
                }
            } else {
                all_items.push(path.clone());
            }
        }

        // 并发处理所有项目
        all_items.par_iter().for_each(|path| {
            if let Err(e) = remove_path_recursive(path, &stats) {
                eprintln!("删除失败 '{}': {}", path.display(), e);
                stats.increment_errors();
            }
        });

        // 清理空的顶层目录
        for path in paths {
            if path.is_dir() {
                if let Err(e) = fs::remove_dir(path) {
                    if e.kind() != io::ErrorKind::NotFound {
                        eprintln!("删除目录失败 '{}': {}", path.display(), e);
                        stats.increment_errors();
                    }
                } else {
                    stats.increment_dirs();
                }
            }
        }
    }

    stats.print_summary();
    Ok(())
}

/// 简化方案2: 混合策略 - 小目录用remove_dir_all，大目录用并发
fn hybrid_remove(path: &Path, stats: &Arc<Stats>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;

    if !metadata.is_dir() {
        // 文件直接删除
        return remove_file_with_stats(path, stats);
    }

    // 估算目录大小
    let dir_size = estimate_dir_size(path)?;

    if dir_size < 100 {
        // 小目录，直接用系统调用
        remove_dir_all_with_stats(path, stats)
    } else {
        // 大目录，使用并发策略
        parallel_remove_large_dir(path, stats)
    }
}

fn estimate_dir_size(path: &Path) -> io::Result<usize> {
    let mut count = 0;
    let mut dirs_to_check = vec![path.to_path_buf()];
    let max_check = 1000; // 最多检查1000个项目来估算

    while let Some(dir) = dirs_to_check.pop() {
        if count > max_check {
            break;
        }

        for entry in fs::read_dir(&dir)? {
            if let Ok(entry) = entry {
                count += 1;
                if count > max_check {
                    break;
                }

                if entry.file_type()?.is_dir() {
                    dirs_to_check.push(entry.path());
                }
            }
        }
    }

    Ok(count)
}

fn remove_dir_all_with_stats(path: &Path, stats: &Arc<Stats>) -> io::Result<()> {
    // 先统计信息
    count_items_in_dir(path, stats)?;
    // 然后删除
    fs::remove_dir_all(path)?;
    Ok(())
}

fn count_items_in_dir(path: &Path, stats: &Arc<Stats>) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        if let Ok(entry) = entry {
            let entry_path = entry.path();
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                stats.increment_dirs();
                count_items_in_dir(&entry_path, stats)?;
            } else {
                stats.increment_files();
                if let Ok(metadata) = entry.metadata() {
                    stats.add_bytes(metadata.len());
                }
            }
        }
    }
    Ok(())
}

fn parallel_remove_large_dir(path: &Path, stats: &Arc<Stats>) -> io::Result<()> {
    let entries: Vec<_> = fs::read_dir(path)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();

    // 并发处理所有子项
    entries.par_iter().for_each(|entry_path| {
        if let Err(e) = remove_path_recursive(entry_path, stats) {
            eprintln!("删除失败 '{}': {}", entry_path.display(), e);
            stats.increment_errors();
        }
    });

    // 删除空目录
    match fs::remove_dir(path) {
        Ok(_) => stats.increment_dirs(),
        Err(e) => {
            eprintln!("删除目录失败 '{}': {}", path.display(), e);
            stats.increment_errors();
        }
    }

    Ok(())
}

fn remove_path_recursive(path: &Path, stats: &Arc<Stats>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;

    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        hybrid_remove(path, stats)
    } else {
        remove_file_with_stats(path, stats)
    }
}

fn remove_file_with_stats(path: &Path, stats: &Arc<Stats>) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        stats.add_bytes(metadata.len());
    }

    fs::remove_file(path)?;
    stats.increment_files();
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: {} <路径>...", args[0]);
        return;
    }

    let paths: Vec<PathBuf> = args[1..].iter().map(PathBuf::from).collect();
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    println!("开始删除 (使用 {} 个线程)...", num_threads);
    let start_time = Instant::now();

    // 设置rayon线程池
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build_global()
        .unwrap();

    if let Err(e) = parallel_remove_top_level(&paths, num_threads) {
        eprintln!("删除失败: {}", e);
    }

    println!("总耗时: {:.2}秒", start_time.elapsed().as_secs_f64());
}
