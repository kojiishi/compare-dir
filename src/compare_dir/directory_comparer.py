import argparse
import filecmp
import concurrent.futures
from collections import deque
import logging
import os
from pathlib import Path
import time
import sys

from compare_dir import __version__

class FileComparisonResult:
    """
    A class to store the comparison result for a single file.
    """
    # Classification constants
    ONLY_IN_DIR1 = 1
    ONLY_IN_DIR2 = 2
    IN_BOTH = 3

    def __init__(self, relative_path: str, classification: int):
        self.relative_path = str(relative_path)
        self.classification = classification # Should be one of the constants
        # Comparison results stored as bool/int. None means not applicable.
        self.modified_time_comparison: int | None = None  # 1: dir1 > dir2, -1: dir2 > dir1, 0: same
        self.size_comparison: int | None = None           # 1: dir1 > dir2, -1: dir2 > dir1, 0: same
        self.is_content_same: bool | None = None          # True: same, False: different

    def is_identical(self):
        """Returns True if the file exists in both directories and is identical."""
        return (self.classification == self.IN_BOTH and
                self.modified_time_comparison == 0 and
                self.size_comparison == 0 and
                self.is_content_same is True)

    def to_string(self, dir1_name: str = 'dir1', dir2_name: str = 'dir2'):
        """String representation of the file comparison result."""
        list = []
        if self.classification == self.ONLY_IN_DIR1:
            list.append(f"Only in {dir1_name}")
        elif self.classification == self.ONLY_IN_DIR2:
            list.append(f"Only in {dir2_name}")
        elif self.classification == self.IN_BOTH:
            # list.append("Exists in both directories")
            pass
        else:
            list.append("Unknown")

        if self.modified_time_comparison is None:
            list.append("Modified time not applicable")
        elif self.modified_time_comparison > 0:
            list.append(f"{dir1_name} is newer")
        elif self.modified_time_comparison < 0:
            list.append(f"{dir2_name} is newer")

        if self.size_comparison is None:
            pass
        elif self.size_comparison > 0:
            list.append(f"Size of {dir1_name} is larger")
        elif self.size_comparison < 0:
            list.append(f"Size of {dir2_name} is larger")

        if self.is_content_same is None:
            if not self.size_comparison:
                list.append("Content comparison not applicable")
        elif not self.is_content_same:
                list.append("Content differ")

        details = ", ".join(list)
        return f"{self.relative_path}: {details}"

class ComparisonSummary:
    """Collects and prints a summary of comparison results."""
    def __init__(self):
        self.in_both = 0
        self.only_in_dir1 = 0
        self.only_in_dir2 = 0
        self.dir1_newer = 0
        self.dir2_newer = 0
        self.same_time_diff_size = 0
        self.same_time_size_diff_content = 0

    def update(self, result: FileComparisonResult):
        """Updates the summary counters based on a single comparison result."""
        if result.classification == FileComparisonResult.ONLY_IN_DIR1:
            self.only_in_dir1 += 1
        elif result.classification == FileComparisonResult.ONLY_IN_DIR2:
            self.only_in_dir2 += 1
        elif result.classification == FileComparisonResult.IN_BOTH:
            self.in_both += 1
            if result.modified_time_comparison == 1:
                self.dir1_newer += 1
            elif result.modified_time_comparison == -1:
                self.dir2_newer += 1
            elif result.size_comparison != 0:
                self.same_time_diff_size += 1
            elif result.is_content_same is False:
                self.same_time_size_diff_content += 1

    def print(self, dir1_name: str, dir2_name: str):
        """Prints the formatted summary."""
        print(f"Files in both: {self.in_both}")
        print(f"Files only in {dir1_name}: {self.only_in_dir1}")
        print(f"Files only in {dir2_name}: {self.only_in_dir2}")
        print(f"Files in both ({dir1_name} is newer): {self.dir1_newer}")
        print(f"Files in both ({dir2_name} is newer): {self.dir2_newer}")
        print(f"Files in both (same time, different size): {self.same_time_diff_size}")
        print(f"Files in both (same time and size, different content): {self.same_time_size_diff_content}")

class DirectoryComparer:
    """
    Compares two directories and yields FileComparisonResult objects for each file.
    """
    def __init__(self, dir1: Path | str, dir2: Path | str, max_workers: int = 0):
        self.dir1 = Path(dir1)
        self.dir2 = Path(dir2)
        self._max_workers = max_workers if max_workers > 0 else None

    def __enter__(self):
        self.executor = concurrent.futures.ThreadPoolExecutor(max_workers=self._max_workers)
        return self

    def __exit__(self, exc_type, exc_value, traceback):
        # When exiting the 'with' block, shut down the executor.
        # If the exit is due to a KeyboardInterrupt, we want to shut down
        # quickly without waiting for running tasks.
        if exc_type is KeyboardInterrupt:
            print("\nInterrupted by user. Shutting down...", file=sys.stderr)
            self.executor.shutdown(wait=False, cancel_futures=True)
        else:
            self.executor.shutdown(wait=True)

    @staticmethod
    def _compare(value1, value2) -> int:
        """Compares two modification times and returns -1, 0, or 1."""
        if value1 > value2:
            return 1
        if value2 > value1:
            return -1
        return 0

    @staticmethod
    def _get_files_in_directory(directory_path: Path | str) -> dict[str, Path]:
        """
        Walks through a directory and returns a dictionary of relative paths to absolute paths.
        """
        base_directory = Path(directory_path)
        file_map = {}
        for dirpath, _, filenames in os.walk(base_directory):
            current_dir_path = Path(dirpath)
            for filename in filenames:
                full_path = current_dir_path / filename
                relative_path = full_path.relative_to(base_directory)
                file_map[str(relative_path)] = full_path
        return file_map

    def _compare_file_pair(self, rel_path: str, dir1_files: dict[str, Path], dir2_files: dict[str, Path]) -> FileComparisonResult:
        """Compares a single pair of files that exist in both directories."""
        result = FileComparisonResult(rel_path, FileComparisonResult.IN_BOTH)
        file1_path = dir1_files[rel_path]
        file2_path = dir2_files[rel_path]

        # Compare modified times and sizes.
        stat1 = file1_path.stat()
        stat2 = file2_path.stat()
        result.modified_time_comparison = DirectoryComparer._compare(stat1.st_mtime, stat2.st_mtime)
        result.size_comparison = DirectoryComparer._compare(stat1.st_size, stat2.st_size)

        if result.size_comparison == 0:
            # If size is the same, check file content
            logging.info("Comparing content: %s", rel_path)
            result.is_content_same = filecmp.cmp(
                file1_path, file2_path, shallow=False
            )
        return result

    def __iter__(self):
        """Yields FileComparisonResult objects for each file."""
        start_time = time.monotonic()
        logging.info("Scanning directories: %s %s", self.dir1, self.dir2)
        future1 = self.executor.submit(self._get_files_in_directory, self.dir1)
        future2 = self.executor.submit(self._get_files_in_directory, self.dir2)

        dir1_files = future1.result()
        dir2_files = future2.result()
        logging.info("Scanning directories finished in %s.", time.strftime('%H:%M:%S', time.gmtime(time.monotonic() - start_time)))

        all_files = sorted(set(dir1_files.keys()) | set(dir2_files.keys()))
        # A deque to hold futures and pre-computed results in sorted order.
        pending_queue = deque()

        # This loop will both queue work and yield completed results.
        for rel_path in all_files:
            in_dir1 = rel_path in dir1_files
            in_dir2 = rel_path in dir2_files

            if in_dir1 and not in_dir2:
                pending_queue.append(FileComparisonResult(rel_path, FileComparisonResult.ONLY_IN_DIR1))
            elif not in_dir1 and in_dir2:
                pending_queue.append(FileComparisonResult(rel_path, FileComparisonResult.ONLY_IN_DIR2))
            else: # Exists in both
                future = self.executor.submit(self._compare_file_pair, rel_path, dir1_files, dir2_files)
                pending_queue.append(future)

            # Try to yield from the front of the queue if the result is ready.
            # This allows us to yield results while still queuing up more work.
            yield from self.yield_from_queue(pending_queue, stop_at_running_task=True)

        # After the main loop, yield any remaining results from the queue.
        yield from self.yield_from_queue(pending_queue)

    @staticmethod
    def yield_from_queue(queue: deque, stop_at_running_task: bool = False):
        while queue:
            first_item = queue[0]
            if isinstance(first_item, concurrent.futures.Future):
                # If it's a future, check if it's done without blocking.
                if stop_at_running_task and first_item.running():
                    # The first item in the queue is not ready, so we can't
                    # yield it yet. Break and queue more work.
                    break
                queue.popleft()
                yield first_item.result()
                continue
            assert isinstance(first_item, FileComparisonResult)
            queue.popleft()
            yield first_item

def main():
    """
    Main function to parse arguments and print comparison results.
    """
    parser = argparse.ArgumentParser(description="Compare two directories.")
    parser.add_argument("dir1", help="Path to the first directory.")
    parser.add_argument("dir2", help="Path to the second directory.")
    parser.add_argument("-p", "--parallel", type=int, default=0, help="Number of parallel threads for file comparison. If 0, uses the default.")
    parser.add_argument("-v", "--verbose", action="store_true", help="Enable verbose logging to stderr.")
    parser.add_argument("-V", "--version", action="version", version=f"%(prog)s {__version__}")
    args = parser.parse_args()

    dir1_path = Path(args.dir1)
    dir2_path = Path(args.dir2)

    if args.verbose:
        logging.basicConfig(level=logging.INFO,
                            format='%(levelname)s: %(message)s',
                            stream=sys.stderr)

    if not dir1_path.is_dir():
        print(f"Error: Directory not found at '{args.dir1}'")
        return
    if not dir2_path.is_dir():
        print(f"Error: Directory not found at '{args.dir2}'")
        return

    start_time = time.monotonic()
    summary = ComparisonSummary()
    try:
        with DirectoryComparer(args.dir1, args.dir2, max_workers=args.parallel) as comparer:
            for result in comparer:
                summary.update(result)
                if result.is_identical():
                    continue
                print(result.to_string(args.dir1, args.dir2))

        # Print the summary only if the comparison completes without interruption.
        print("\n--- Comparison Summary ---")
        summary.print(args.dir1, args.dir2)
    except KeyboardInterrupt:
        # The __exit__ method of DirectoryComparer handles the shutdown.
        pass
    finally:
        print(f"Comparison finished in {time.strftime('%H:%M:%S', time.gmtime(time.monotonic() - start_time))}.")

if __name__ == "__main__":
    main()
