# ADR 0002: eframe with direct egui-wgpu callbacks

Status: accepted, 2026-08-12

## Context

The application needs ordinary desktop controls and unusually fluid custom biofeedback
graphics. Owning a hand-written winit event loop immediately would slow the first vertical
slice, while using only egui primitives would not exercise the intended GPU architecture.

## Decision

Use eframe's wgpu backend for the window, input, accessibility, persistence, and egui
rendering. Insert custom retained wgpu pipelines with `egui_wgpu::CallbackTrait` inside
allocated egui regions. Use the wgpu version re-exported by the selected egui-wgpu version.

## Consequences

Custom visuals share the same device, queue, surface, and render pass as the UI. The
application can later take ownership of winit without rewriting `kasina-render` if an
observed eframe limitation justifies the additional complexity.

