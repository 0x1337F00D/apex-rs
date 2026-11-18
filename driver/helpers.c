// SPDX-License-Identifier: GPL-2.0-only
/*
 * C helpers for apex-rs: kernel macros that Rust cannot call through bindgen.
 * Keep this file minimal; all driver logic is in Rust.
 */
#include <linux/mm.h>

void apex_rs_vma_set_device_prot(struct vm_area_struct *vma);

/* Give a VMA device-memory (uncached) attributes, as pci_mmap_resource_range() does. */
void apex_rs_vma_set_device_prot(struct vm_area_struct *vma)
{
	vma->vm_page_prot = pgprot_device(vma->vm_page_prot);
}
