// Copyright (c) 2021 by Rivos Inc.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

//! # Page table management for HS mode on Risc-V.
//!
//! ## Key types
//!
//! - `Page` is the basic building block, representing pages of host supervisor memory. Provided by
//!   the `riscv-pages` crate.
//! - `PlatformPageTable` is a top-level page table structures used to manipulate address translation
//! and protection.
//! - `PageTable` provides a generic implementation of a single level of multi-level translation.
//! - `Sv48x4`, `Sv48`, etc. define standard RISC-V translation modes for 1st or 2nd-stage translation
//! tables.
//! - `PageTracker` - Contains information about active VMs (page owners), manages allocation of
//! unique owner IDs, and per-page state such as current and previous owner. This is system-wide
//! state updated whenever a page owner changes or a VM starts or stops.
//! - `PageMap` - Per-page state (tracks the owner).
//! - `HypPageAlloc` - Initial manager of physical memory. The hypervisor allocates pages from
//! here to store local state. It's turned in to a `PageTracker` and a pool of ram for the host VM.
//! - `HwMemMap` - Map of system memory, used to determine address ranges to create `Page`s from.
//!
//! ## Initialization
//!
//! `HwMemMap` -> `HypPageAlloc` ---> `PageTracker`
//!                                 \
//!                                  -------> `SequentialPages`
//!
//! ## Safety
//!
//! Safe interfaces are exposed by giving each `PlatformPageTable` ownership of the pages used to
//! construct the page tables. In this way the pages can be manipulated as needed, but only by the
//! owning page table. The details of managing the pages are contained in the page table.
//!
//! Note that leaf pages mapped into the table are assumed to never be safe to "own", they are
//! implicitly shared with the user of the page table (the entity on the other end of that stage of
//! address translation). Interacting directly with memory currently mapped to a VM will lead to
//! pain so the interfaces don't support that.
#![no_std]
#![feature(allocator_api, let_chains)]

extern crate alloc;

mod hw_mem_map;
mod page_info;
/// Implements a linked-list of pages using `PageTracker`.
pub mod page_list;
mod page_table;
/// Handles tracking the owner and state of each page.
pub mod page_tracking;
/// Provides access to the fields of a riscv PTE.
mod pte;
/// Interfaces to build and manage sv48 page tables for S and U mode access.
mod sv48;
/// Interfaces to build and manage sv48x4 page tables for VMs.
pub mod sv48x4;
/// Provides low-level TLB management functions such as fencing.
pub mod tlb;

pub use hw_mem_map::Error as MemMapError;
pub use hw_mem_map::Result as MemMapResult;
pub use hw_mem_map::{HwMemMap, HwMemMapBuilder, HwMemRegion, HwMemRegionType, HwReservedMemType};
pub use page_info::MAX_PAGE_OWNERS;
pub use page_list::{LockedPageList, PageList};
pub use page_table::Error as PageTableError;
pub use page_table::Result as PageTableResult;
pub use page_table::{
    FirstStagePageTable, GuestStagePageTable, PageTableMapper, PagingMode, PlatformPageTable,
};
pub use page_tracking::Error as PageTrackingError;
pub use page_tracking::Result as PageTrackingResult;
pub use page_tracking::{HypPageAlloc, PageTracker};
pub use sv48::Sv48;
pub use sv48x4::Sv48x4;
pub use tlb::TlbVersion;

#[cfg(test)]
#[macro_use]
extern crate std;

#[cfg(test)]
mod tests {
    use alloc::alloc::Global;
    use alloc::vec::Vec;
    use riscv_pages::*;
    use std::{mem, slice};

    use super::page_table::*;
    use super::sv48::Sv48;
    use super::sv48x4::Sv48x4;
    use super::*;

    struct StubState {
        root_pages: SequentialPages<InternalClean>,
        pte_pages: SequentialPages<InternalClean>,
        page_tracker: PageTracker,
        host_pages: Vec<SequentialPages<ConvertedClean>, Global>,
    }

    fn stub_sys_memory() -> StubState {
        const ONE_MEG: usize = 1024 * 1024;
        const MEM_ALIGN: usize = 2 * ONE_MEG;
        const MEM_SIZE: usize = 256 * ONE_MEG;
        let backing_mem = vec![0u8; MEM_SIZE + MEM_ALIGN];
        let aligned_pointer = unsafe {
            // Not safe - just a test
            backing_mem
                .as_ptr()
                .add(backing_mem.as_ptr().align_offset(MEM_ALIGN))
        };
        let start_pa = RawAddr::supervisor(aligned_pointer as u64);
        let hw_map = unsafe {
            // Not safe - just a test
            HwMemMapBuilder::new(Sv48x4::TOP_LEVEL_ALIGN)
                .add_memory_region(start_pa, MEM_SIZE.try_into().unwrap())
                .unwrap()
                .build()
        };
        let mut hyp_mem = HypPageAlloc::new(hw_map, Global);
        let root_pages =
            hyp_mem.take_pages_for_host_state_with_alignment(4, Sv48x4::TOP_LEVEL_ALIGN);
        let pte_pages = hyp_mem.take_pages_for_host_state(3);
        let (page_tracker, host_pages) = PageTracker::from(hyp_mem, Sv48x4::TOP_LEVEL_ALIGN);
        // Leak the backing ram so it doesn't get freed
        std::mem::forget(backing_mem);
        StubState {
            root_pages,
            pte_pages,
            page_tracker,
            host_pages,
        }
    }

    #[test]
    fn map_and_unmap_sv48x4() {
        let state = stub_sys_memory();

        let page_tracker = state.page_tracker;
        let mut host_pages = state.host_pages.into_iter().flatten();
        let id = page_tracker.add_active_guest().unwrap();
        let guest_page_table: PlatformPageTable<Sv48x4> =
            PlatformPageTable::new(state.root_pages, id, page_tracker.clone())
                .expect("creating sv48x4");

        let pages_to_map = [host_pages.next().unwrap(), host_pages.next().unwrap()];
        let page_addrs: Vec<SupervisorPageAddr> = pages_to_map.iter().map(|p| p.addr()).collect();
        let mut pte_pages = state.pte_pages.into_iter();
        let gpa_base = PageAddr::new(RawAddr::guest(0x8000_0000, PageOwnerId::host())).unwrap();
        let mapper = guest_page_table
            .map_range(gpa_base, PageSize::Size4k, 2, &mut || pte_pages.next())
            .unwrap();
        for (page, gpa) in pages_to_map.into_iter().zip(gpa_base.iter_from()) {
            // Write to the page so that we can test if it's retained later.
            unsafe {
                // Not safe - just a test
                let slice = slice::from_raw_parts_mut(
                    page.addr().bits() as *mut u64,
                    page.size() as usize / mem::size_of::<u64>(),
                );
                slice[0] = 0xdeadbeef;
            }
            let mappable = page_tracker.assign_page_for_mapping(page, id).unwrap();
            assert!(mapper.map_page(gpa, mappable).is_ok());
        }
        let version = TlbVersion::new();
        guest_page_table
            .invalidate_range::<Page<Invalidated>>(gpa_base, PageSize::Size4k, 2)
            .unwrap()
            .for_each(|invalidated| page_tracker.convert_page(invalidated, version).unwrap());
        let version = version.increment();
        let mut converted_pages = guest_page_table
            .get_converted_range::<Page<ConvertedDirty>>(gpa_base, PageSize::Size4k, 2, version)
            .unwrap();
        let dirty_page = converted_pages.next().unwrap();
        assert_eq!(dirty_page.addr(), page_addrs[0]);
        assert_eq!(dirty_page.get_u64(0).unwrap(), 0xdeadbeef);
        let clean_page = converted_pages.next().unwrap().clean();
        assert_eq!(clean_page.addr(), page_addrs[1]);
        assert_eq!(clean_page.get_u64(0).unwrap(), 0);
    }

    #[test]
    fn map_and_unmap_sv48() {
        let state = stub_sys_memory();

        let page_tracker = state.page_tracker;
        let mut host_pages = state.host_pages.into_iter().flatten();
        let id = page_tracker.add_active_guest().unwrap();
        let guest_page_table: PlatformPageTable<Sv48> =
            PlatformPageTable::new(state.root_pages, id, page_tracker.clone())
                .expect("creating sv48");

        let pages_to_map = [host_pages.next().unwrap(), host_pages.next().unwrap()];
        let page_addrs: Vec<SupervisorPageAddr> = pages_to_map.iter().map(|p| p.addr()).collect();
        let mut pte_pages = state.pte_pages.into_iter();
        let gpa_base = PageAddr::new(RawAddr::supervisor_virt(0x8000_0000)).unwrap();
        let mapper = guest_page_table
            .map_range(gpa_base, PageSize::Size4k, 2, &mut || pte_pages.next())
            .unwrap();
        for (page, gpa) in pages_to_map.into_iter().zip(gpa_base.iter_from()) {
            // Write to the page so that we can test if it's retained later.
            unsafe {
                // Not safe - just a test
                let slice = slice::from_raw_parts_mut(
                    page.addr().bits() as *mut u64,
                    page.size() as usize / mem::size_of::<u64>(),
                );
                slice[0] = 0xdeadbeef;
            }
            let mappable = page_tracker.assign_page_for_mapping(page, id).unwrap();
            assert!(mapper.map_page(gpa, mappable).is_ok());
        }
        let version = TlbVersion::new();
        guest_page_table
            .invalidate_range::<Page<Invalidated>>(gpa_base, PageSize::Size4k, 2)
            .unwrap()
            .for_each(|invalidated| page_tracker.convert_page(invalidated, version).unwrap());
        let version = version.increment();
        let mut converted_pages = guest_page_table
            .get_converted_range::<Page<ConvertedDirty>>(gpa_base, PageSize::Size4k, 2, version)
            .unwrap();
        let dirty_page = converted_pages.next().unwrap();
        assert_eq!(dirty_page.addr(), page_addrs[0]);
        assert_eq!(dirty_page.get_u64(0).unwrap(), 0xdeadbeef);
        let clean_page = converted_pages.next().unwrap().clean();
        assert_eq!(clean_page.addr(), page_addrs[1]);
        assert_eq!(clean_page.get_u64(0).unwrap(), 0);
    }

    #[test]
    fn page_list() {
        let state = stub_sys_memory();
        let first_page_addr = state.pte_pages.base();
        {
            let mut pte_list = PageList::new(state.page_tracker.clone());
            for p in state.pte_pages {
                pte_list.push(p).unwrap();
            }
            // Not safe -- just a test.
            let already_linked: Page<InternalClean> = unsafe { Page::new(first_page_addr) };
            assert!(pte_list.push(already_linked).is_err());
        }
        let mut new_list = PageList::new(state.page_tracker);
        for p in state.root_pages {
            new_list.push(p).unwrap();
        }
        let was_linked: Page<InternalClean> = unsafe { Page::new(first_page_addr) };
        new_list.push(was_linked).unwrap();
    }
}
