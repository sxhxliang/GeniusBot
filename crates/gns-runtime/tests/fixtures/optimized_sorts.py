"""
Optimized Descending Sort Algorithms
=====================================
Three classic sorting algorithms optimized for descending order:
1. Quick Sort - with median-of-three pivot and insertion sort for small arrays
2. Bubble Sort - with early termination and reduced comparison range
3. Merge Sort - with insertion sort for small subarrays and single auxiliary array
"""

from typing import List
import random


# =============================================================================
# 1. QUICK SORT (Descending) - Optimized
# =============================================================================

def quick_sort_desc(arr: List[int]) -> List[int]:
    """
    Optimized quick sort in descending order.
    
    Optimizations:
    - Median-of-three pivot selection to avoid worst-case O(n²) on sorted data
    - Insertion sort for small subarrays (n < 16) to reduce overhead
    - In-place partitioning to minimize memory usage
    """
    if len(arr) <= 1:
        return arr.copy()
    
    result = arr.copy()
    _quick_sort_helper(result, 0, len(result) - 1)
    return result


def _quick_sort_helper(arr: List[int], low: int, high: int) -> None:
    """In-place quick sort helper with optimizations."""
    # Use insertion sort for small subarrays
    if high - low < 16:
        _insertion_sort_desc(arr, low, high)
        return
    
    # Partition and recursively sort
    pivot_idx = _partition_desc(arr, low, high)
    _quick_sort_helper(arr, low, pivot_idx - 1)
    _quick_sort_helper(arr, pivot_idx + 1, high)


def _median_of_three(arr: List[int], low: int, high: int) -> int:
    """Select median of first, middle, and last elements as pivot."""
    mid = (low + high) // 2
    
    # Sort the three values and return the median index
    if arr[low] < arr[mid]:
        arr[low], arr[mid] = arr[mid], arr[low]
    if arr[low] < arr[high]:
        arr[low], arr[high] = arr[high], arr[low]
    if arr[mid] < arr[high]:
        arr[mid], arr[high] = arr[high], arr[mid]
    
    return mid


def _partition_desc(arr: List[int], low: int, high: int) -> int:
    """
    Lomuto partition scheme for descending order.
    Elements >= pivot go to the left, elements < pivot go to the right.
    """
    # Use median-of-three for better pivot selection
    pivot_idx = _median_of_three(arr, low, high)
    arr[pivot_idx], arr[high] = arr[high], arr[pivot_idx]
    pivot = arr[high]
    
    # i tracks the boundary of elements >= pivot
    i = low - 1
    
    for j in range(low, high):
        if arr[j] >= pivot:  # >= for descending order
            i += 1
            arr[i], arr[j] = arr[j], arr[i]
    
    # Place pivot in correct position
    arr[i + 1], arr[high] = arr[high], arr[i + 1]
    return i + 1


def _insertion_sort_desc(arr: List[int], low: int, high: int) -> None:
    """Insertion sort for small subarrays (descending order)."""
    for i in range(low + 1, high + 1):
        key = arr[i]
        j = i - 1
        while j >= low and arr[j] < key:  # < for descending order
            arr[j + 1] = arr[j]
            j -= 1
        arr[j + 1] = key


# =============================================================================
# 2. BUBBLE SORT (Descending) - Optimized
# =============================================================================

def bubble_sort_desc(arr: List[int]) -> List[int]:
    """
    Optimized bubble sort in descending order.
    
    Optimizations:
    - Early termination if no swaps occur (array is sorted)
    - Track last swap position to reduce comparison range
    - Skip already-sorted elements at the end
    """
    if len(arr) <= 1:
        return arr.copy()
    
    result = arr.copy()
    n = len(result)
    
    while n > 1:
        new_n = 0  # Track where the last swap occurred
        
        for i in range(1, n):
            if result[i - 1] < result[i]:  # < for descending order
                result[i - 1], result[i] = result[i], result[i - 1]
                new_n = i  # Update last swap position
        
        # Elements after new_n are already in place
        n = new_n
    
    return result


# =============================================================================
# 3. MERGE SORT (Descending) - Optimized
# =============================================================================

def merge_sort_desc(arr: List[int]) -> List[int]:
    """
    Optimized merge sort in descending order.
    
    Optimizations:
    - Insertion sort for small subarrays (n < 16)
    - Single auxiliary array to reduce memory allocation
    - Avoid unnecessary copying
    """
    if len(arr) <= 1:
        return arr.copy()
    
    result = arr.copy()
    aux = [0] * len(result)  # Pre-allocate auxiliary array
    _merge_sort_helper(result, aux, 0, len(result) - 1)
    return result


def _merge_sort_helper(arr: List[int], aux: List[int], low: int, high: int) -> None:
    """Merge sort helper using pre-allocated auxiliary array."""
    # Use insertion sort for small subarrays
    if high - low < 16:
        _insertion_sort_desc(arr, low, high)
        return
    
    mid = (low + high) // 2
    _merge_sort_helper(arr, aux, low, mid)
    _merge_sort_helper(arr, aux, mid + 1, high)
    
    # Skip merge if already sorted (optimization for partially sorted data)
    if arr[mid] >= arr[mid + 1]:
        return
    
    _merge_desc(arr, aux, low, mid, high)


def _merge_desc(arr: List[int], aux: List[int], low: int, mid: int, high: int) -> None:
    """Merge two sorted subarrays in descending order."""
    # Copy to auxiliary array
    for k in range(low, high + 1):
        aux[k] = arr[k]
    
    i = low      # Pointer for left subarray
    j = mid + 1  # Pointer for right subarray
    
    # Merge back to original array in descending order
    for k in range(low, high + 1):
        if i > mid:
            arr[k] = aux[j]
            j += 1
        elif j > high:
            arr[k] = aux[i]
            i += 1
        elif aux[i] >= aux[j]:  # >= for descending order
            arr[k] = aux[i]
            i += 1
        else:
            arr[k] = aux[j]
            j += 1


# =============================================================================
# BONUS: Python's Built-in Timsort (for comparison)
# =============================================================================

def timsort_desc(arr: List[int]) -> List[int]:
    """
    Python's built-in Timsort in descending order.
    This is the most efficient general-purpose sort in practice.
    """
    return sorted(arr, reverse=True)


# =============================================================================
# TESTING AND BENCHMARKING
# =============================================================================

def test_all_sorts():
    """Test all sorting algorithms with various inputs."""
    test_cases = [
        [],                           # Empty
        [1],                          # Single element
        [1, 2],                       # Two elements
        [3, 1, 4, 1, 5, 9, 2, 6],    # Random
        [1, 2, 3, 4, 5],             # Already sorted (ascending)
        [5, 4, 3, 2, 1],             # Already sorted (descending)
        [2, 2, 2, 2],                # All same
        [-3, -1, -4, -1, -5],        # Negative numbers
        list(range(100, 0, -1)),     # Large descending
        list(range(1, 101)),         # Large ascending
    ]
    
    algorithms = [
        ("Quick Sort", quick_sort_desc),
        ("Bubble Sort", bubble_sort_desc),
        ("Merge Sort", merge_sort_desc),
        ("Timsort", timsort_desc),
    ]
    
    print("=" * 60)
    print("SORTING ALGORITHM TESTS")
    print("=" * 60)
    
    for name, sort_func in algorithms:
        print(f"\n{name}:")
        all_passed = True
        
        for i, test in enumerate(test_cases):
            result = sort_func(test)
            expected = sorted(test, reverse=True)
            passed = result == expected
            
            if not passed:
                all_passed = False
                print(f"  Test {i}: FAILED")
                print(f"    Input:    {test[:10]}{'...' if len(test) > 10 else ''}")
                print(f"    Expected: {expected[:10]}{'...' if len(expected) > 10 else ''}")
                print(f"    Got:      {result[:10]}{'...' if len(result) > 10 else ''}")
            else:
                print(f"  Test {i}: PASSED")
        
        status = "✓ ALL PASSED" if all_passed else "✗ SOME FAILED"
        print(f"  {status}")


def benchmark_sorts():
    """Benchmark all sorting algorithms."""
    import time
    
    sizes = [100, 1000, 10000]
    algorithms = [
        ("Quick Sort", quick_sort_desc),
        ("Bubble Sort", bubble_sort_desc),
        ("Merge Sort", merge_sort_desc),
        ("Timsort", timsort_desc),
    ]
    
    print("\n" + "=" * 60)
    print("PERFORMANCE BENCHMARKS")
    print("=" * 60)
    
    for size in sizes:
        print(f"\nArray size: {size}")
        print("-" * 40)
        
        # Generate random array
        test_arr = [random.randint(0, 10000) for _ in range(size)]
        
        for name, sort_func in algorithms:
            # Warm up
            _ = sort_func(test_arr)
            
            # Benchmark
            start = time.perf_counter()
            iterations = 10 if size <= 1000 else 3
            for _ in range(iterations):
                result = sort_func(test_arr)
            elapsed = (time.perf_counter() - start) / iterations
            
            print(f"  {name:15} {elapsed*1000:8.3f} ms")


if __name__ == "__main__":
    test_all_sorts()
    benchmark_sorts()
