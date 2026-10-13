//! Platform-neutral GUI: owns the child window (see native.rs) and a render thread drawing a
//! Vulkan (ash) triangle into it.
// ponytail: fixed 400x300 window, no resize/HiDPI, unwrap() instead of error handling inside the
// render thread (a panic there only kills the GUI thread, never the audio thread).
use crate::native::{Native, API, EXTS, H, W};
use crate::Main;
use ash::{khr, vk, Entry};
use clack_extensions::gui::*;
use clack_plugin::prelude::*;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;

const SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tri.spv"));

pub struct Open {
    native: Arc<Native>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Main<'_> {
    fn stop_render(&self) {
        if let Some(o) = self.gui.borrow_mut().as_mut() {
            o.stop.store(true, Relaxed);
            if let Some(t) = o.thread.take() {
                let _ = t.join();
            }
        }
    }
}

impl PluginGuiImpl for Main<'_> {
    fn is_api_supported(&self, c: GuiConfiguration) -> bool {
        c.api_type == API && !c.is_floating
    }

    fn get_preferred_api(&self) -> Option<GuiConfiguration<'_>> {
        Some(GuiConfiguration {
            api_type: API,
            is_floating: false,
        })
    }

    // The window is created in set_parent(): Win32 child windows need their parent up front.
    fn create(&self, _: GuiConfiguration) -> Result<(), PluginError> {
        Ok(())
    }

    fn destroy(&self) {
        self.stop_render();
        self.gui.borrow_mut().take(); // drops Native -> destroys the window
    }

    fn set_scale(&self, _: f64) -> Result<(), PluginError> {
        Ok(())
    }

    fn get_size(&self) -> Option<GuiSize> {
        Some(GuiSize {
            width: W,
            height: H,
        })
    }

    fn set_size(&self, _: GuiSize) -> Result<(), PluginError> {
        Ok(())
    }

    fn set_parent(&self, window: Window) -> Result<(), PluginError> {
        self.destroy();
        *self.gui.borrow_mut() = Some(Open {
            native: Arc::new(Native::new(&window)?),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        });
        Ok(())
    }

    fn set_transient(&self, _: Window) -> Result<(), PluginError> {
        Ok(())
    }

    fn show(&self) -> Result<(), PluginError> {
        let mut g = self.gui.borrow_mut();
        let o = g
            .as_mut()
            .ok_or(PluginError::Message("set_parent not called"))?;
        o.native.show()?;
        if o.thread.is_none() {
            o.stop.store(false, Relaxed);
            let (native, stop) = (o.native.clone(), o.stop.clone());
            o.thread = Some(std::thread::spawn(move || unsafe {
                render(&native, &stop)
            }));
        }
        Ok(())
    }

    fn hide(&self) -> Result<(), PluginError> {
        self.stop_render();
        if let Some(o) = self.gui.borrow().as_ref() {
            o.native.hide()?;
        }
        Ok(())
    }
}

/// Vulkan setup + render loop. Command buffers are recorded once (the triangle never changes);
/// each frame is acquire -> submit -> present -> queue_wait_idle.
// ponytail: queue_wait_idle per frame instead of frames-in-flight. Add fences when the scene animates.
unsafe fn render(native: &Native, stop: &AtomicBool) {
    let entry = Entry::load().unwrap();
    let exts = EXTS.map(|e| e.as_ptr());
    let inst = entry
        .create_instance(
            &vk::InstanceCreateInfo::default().enabled_extension_names(&exts),
            None,
        )
        .unwrap();
    let surf_i = khr::surface::Instance::new(&entry, &inst);
    let surf = native.surface(&entry, &inst);

    let (pd, qf) = inst
        .enumerate_physical_devices()
        .unwrap()
        .into_iter()
        .find_map(|pd| {
            let fams = inst.get_physical_device_queue_family_properties(pd);
            (0..fams.len() as u32)
                .find(|&i| {
                    fams[i as usize]
                        .queue_flags
                        .contains(vk::QueueFlags::GRAPHICS)
                        && surf_i
                            .get_physical_device_surface_support(pd, i, surf)
                            .unwrap()
                })
                .map(|i| (pd, i))
        })
        .expect("no Vulkan device can present to this X11 window");
    let dev = inst
        .create_device(
            pd,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&[vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(qf)
                    .queue_priorities(&[1.0])])
                .enabled_extension_names(&[khr::swapchain::NAME.as_ptr()]),
            None,
        )
        .unwrap();
    let queue = dev.get_device_queue(qf, 0);

    // swapchain
    let sc_d = khr::swapchain::Device::new(&inst, &dev);
    let caps = surf_i
        .get_physical_device_surface_capabilities(pd, surf)
        .unwrap();
    let fmt = surf_i
        .get_physical_device_surface_formats(pd, surf)
        .unwrap()[0];
    let extent = if caps.current_extent.width == u32::MAX {
        vk::Extent2D {
            width: W,
            height: H,
        }
    } else {
        caps.current_extent
    };
    let sc = sc_d
        .create_swapchain(
            &vk::SwapchainCreateInfoKHR::default()
                .surface(surf)
                .min_image_count(caps.min_image_count.max(2))
                .image_format(fmt.format)
                .image_color_space(fmt.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .pre_transform(caps.current_transform)
                .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                .present_mode(vk::PresentModeKHR::FIFO)
                .clipped(true),
            None,
        )
        .unwrap();
    let images = sc_d.get_swapchain_images(sc).unwrap();

    // render pass
    let attach = [vk::AttachmentDescription::default()
        .format(fmt.format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
    let color_ref =
        [vk::AttachmentReference::default().layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)];
    let dep = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    let rp = dev
        .create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&attach)
                .subpasses(&subpass)
                .dependencies(&dep),
            None,
        )
        .unwrap();

    // pipeline: both shaders live in one SPIR-V module (entry points "vs" / "fs")
    let code = ash::util::read_spv(&mut Cursor::new(SPV)).unwrap();
    let module = dev
        .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
        .unwrap();
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(module)
            .name(c"vs"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(module)
            .name(c"fs"),
    ];
    let layout = dev
        .create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None)
        .unwrap();
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let scissor = [vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    }];
    let blend = [vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let pipe = dev
        .create_graphics_pipelines(
            vk::PipelineCache::null(),
            &[vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vk::PipelineVertexInputStateCreateInfo::default())
                .input_assembly_state(
                    &vk::PipelineInputAssemblyStateCreateInfo::default()
                        .topology(vk::PrimitiveTopology::TRIANGLE_LIST),
                )
                .viewport_state(
                    &vk::PipelineViewportStateCreateInfo::default()
                        .viewports(&viewport)
                        .scissors(&scissor),
                )
                .rasterization_state(
                    &vk::PipelineRasterizationStateCreateInfo::default()
                        .polygon_mode(vk::PolygonMode::FILL)
                        .cull_mode(vk::CullModeFlags::NONE)
                        .line_width(1.0),
                )
                .multisample_state(
                    &vk::PipelineMultisampleStateCreateInfo::default()
                        .rasterization_samples(vk::SampleCountFlags::TYPE_1),
                )
                .color_blend_state(
                    &vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend),
                )
                .layout(layout)
                .render_pass(rp)],
            None,
        )
        .unwrap()[0];

    // per-image view + framebuffer + pre-recorded command buffer
    let pool = dev
        .create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(qf),
            None,
        )
        .unwrap();
    let cmds = dev
        .allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .command_buffer_count(images.len() as u32),
        )
        .unwrap();
    let clear = [vk::ClearValue {
        color: vk::ClearColorValue {
            float32: [0.08, 0.08, 0.1, 1.0],
        },
    }];
    let (mut views, mut fbs) = (vec![], vec![]);
    for (&img, &cmd) in images.iter().zip(&cmds) {
        let view = dev
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(img)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(fmt.format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
            .unwrap();
        let fb = dev
            .create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(rp)
                    .attachments(&[view])
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1),
                None,
            )
            .unwrap();
        dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
            .unwrap();
        dev.cmd_begin_render_pass(
            cmd,
            &vk::RenderPassBeginInfo::default()
                .render_pass(rp)
                .framebuffer(fb)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent,
                })
                .clear_values(&clear),
            vk::SubpassContents::INLINE,
        );
        dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipe);
        dev.cmd_draw(cmd, 3, 1, 0, 0);
        dev.cmd_end_render_pass(cmd);
        dev.end_command_buffer(cmd).unwrap();
        views.push(view);
        fbs.push(fb);
    }

    // frame loop
    let acquire = dev
        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        .unwrap();
    let done = dev
        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        .unwrap();
    while !stop.load(Relaxed) {
        if let Ok((i, _)) = sc_d.acquire_next_image(sc, u64::MAX, acquire, vk::Fence::null()) {
            dev.queue_submit(
                queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&[acquire])
                    .wait_dst_stage_mask(&[vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT])
                    .command_buffers(&[cmds[i as usize]])
                    .signal_semaphores(&[done])],
                vk::Fence::null(),
            )
            .unwrap();
            let _ = sc_d.queue_present(
                queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&[done])
                    .swapchains(&[sc])
                    .image_indices(&[i]),
            );
            dev.queue_wait_idle(queue).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }

    // teardown
    dev.device_wait_idle().unwrap();
    dev.destroy_semaphore(acquire, None);
    dev.destroy_semaphore(done, None);
    dev.destroy_command_pool(pool, None);
    fbs.into_iter()
        .for_each(|f| dev.destroy_framebuffer(f, None));
    views
        .into_iter()
        .for_each(|v| dev.destroy_image_view(v, None));
    dev.destroy_pipeline(pipe, None);
    dev.destroy_pipeline_layout(layout, None);
    dev.destroy_shader_module(module, None);
    dev.destroy_render_pass(rp, None);
    sc_d.destroy_swapchain(sc, None);
    dev.destroy_device(None);
    surf_i.destroy_surface(surf, None);
    inst.destroy_instance(None);
}
