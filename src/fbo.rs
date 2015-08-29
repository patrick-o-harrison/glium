/*!
Contains everything related to the internal handling of framebuffer objects.

*/
/*
Here are the rules taken from the official wiki:

Attachment Completeness

Each attachment point itctxt.framebuffer_objects must be complete according to these rules. Empty attachments
(attachments with no image attached) are complete by default. If an image is attached, it must
adhere to the following rules:

The source object for the image still exists and has the same type it was attached with.
The image has a non-zero width and height (the height of a 1D image is assumed to be 1). The
  width/height must also be less than GL_MAX_FRAMEBUFFER_WIDTH and GL_MAX_FRAMEBUFFER_HEIGHT
  respectively (if GL 4.3/ARB_framebuffer_no_attachments).
The layer for 3D or array textures attachments is less than the depth of the texture. It must
  also be less than GL_MAX_FRAMEBUFFER_LAYERS (if GL 4.3/ARB_framebuffer_no_attachments).
The number of samples must be less than GL_MAX_FRAMEBUFFER_SAMPLES (if
  GL 4.3/ARB_framebuffer_no_attachments).
The image's format must match the attachment point's requirements, as defined above.
  Color-renderable formats for color attachments, etc.

Completeness Rules

These are the rules for framebuffer completeness. The order of these rules matters.

If the target​ of glCheckFramebufferStatus references the Default Framebuffer (ie: FBO object
  number 0 is bound), and the default framebuffer does not exist, then you will get
  GL_FRAMEBUFFER_UNDEFINEZ. If the default framebuffer exists, then you always get
  GL_FRAMEBUFFER_COMPLETE. The rest of the rules apply when an FBO is bound.
All attachments must be attachment complete. (GL_FRAMEBUFFER_INCOMPLETE_ATTACHMENT when false).
There must be at least one image attached to the FBO, or if OpenGL 4.3 or
  ARB_framebuffer_no_attachment is available, the GL_FRAMEBUFFER_DEFAULT_WIDTH and
  GL_FRAMEBUFFER_DEFAULT_HEIGHT parameters of the framebuffer must both be non-zero.
  (GL_FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT when false).
Each draw buffers must either specify color attachment points that have images attached or
  must be GL_NONE. (GL_FRAMEBUFFER_INCOMPLETE_DRAW_BUFFER when false). Note that this test is
  not performed if OpenGL 4.1 or ARB_ES2_compatibility is available.
If the read buffer is set, then it must specify an attachment point that has an image
  attached. (GL_FRAMEBUFFER_INCOMPLETE_READ_BUFFER when false). Note that this test is not
  performed if OpenGL 4.1 or ARB_ES2_compatibility is available.
All images must have the same number of multisample samples.
  (GL_FRAMEBUFFER_INCOMPLETE_MULTISAMPLE when false).
If a layered image is attached to one attachment, then all attachments must be layered
  attachments. The attached layers do not have to have the same number of layers, nor do the
  layers have to come from the same kind of texture (a cubemap color texture can be paired
  with an array depth texture) (GL_FRAMEBUFFER_INCOMPLETE_LAYER_TARGETS when false).

*/
use std::collections::HashMap;
use std::cmp;
use std::mem;
use std::cell::RefCell;
use std::marker::PhantomData;

use smallvec::SmallVec;

use CapabilitiesSource;
use ContextExt;
use GlObject;
use TextureExt;

use texture::CubeLayer;
use texture::TextureAnyImage;
use texture::TextureAnyMipmap;
use framebuffer::RenderBufferAny;

use gl;
use context::CommandContext;
use version::Version;
use version::Api;

/// Returns true if the backend supports attachments with varying dimensions.
///
/// If this function returns `true` and you pass attachments with different dimensions, the
/// intersection between all the attachments will be used. If this function returns `false`, you'll
/// get an error instead.
pub fn is_dimensions_mismatch_supported<C>(context: &C) -> bool where C: CapabilitiesSource {
    context.get_version() >= &Version(Api::Gl, 3, 0) ||
    context.get_version() >= &Version(Api::GlEs, 2, 0) ||
    context.get_extensions().gl_arb_framebuffer_object
}

/// Represents the attachments to use for an OpenGL framebuffer.
#[derive(Clone)]
pub enum FramebufferAttachments<'a> {
    /// Each attachment is a single image.
    Regular(FramebufferSpecificAttachments<RegularAttachment<'a>>),

    /// Each attachment is a layer of images.
    Layered(FramebufferSpecificAttachments<LayeredAttachment<'a>>),
}

/// Describes a single non-layered framebuffer attachment.
#[derive(Copy, Clone)]
pub enum RegularAttachment<'a> {
    /// A texture.
    Texture(TextureAnyImage<'a>),
    /// A renderbuffer.
    RenderBuffer(&'a RenderBufferAny),
}

/// Describes a single layered framebuffer attachment.
#[derive(Copy, Clone)]
pub struct LayeredAttachment<'a>(TextureAnyMipmap<'a>);

/// Depth and/or stencil attachment to use.
#[derive(Copy, Clone)]
pub enum DepthStencilAttachments<T> {
    /// No depth or stencil buffer.
    None,

    /// A depth attachment.
    DepthAttachment(T),

    /// A stencil attachment.
    StencilAttachment(T),

    /// A depth attachment and a stencil attachment.
    DepthAndStencilAttachments(T, T),

    /// A single attachment that serves as both depth and stencil buffer.
    DepthStencilAttachment(T),
}

/// Represents the attachments to use for an OpenGL framebuffer.
#[derive(Clone)]
pub struct FramebufferSpecificAttachments<T> {
    /// List of color attachments. The first parameter of the tuple is the index, and the
    /// second element is the attachment.
    pub colors: SmallVec<[(u32, T); 5]>,

    /// The depth and/or stencil attachment to use.
    pub depth_stencil: DepthStencilAttachments<T>,
}

impl<'a> FramebufferAttachments<'a> {
    /// After building a `FramebufferAttachments` struct, you must use this function
    /// to "compile" the attachments and make sure that they are valid together.
    #[inline]
    pub fn validate(self) -> Result<ValidatedAttachments<'a>, ValidationError> {
        match self {
            FramebufferAttachments::Regular(a) => FramebufferAttachments::validate_regular(a),
            FramebufferAttachments::Layered(a) => FramebufferAttachments::validate_layered(a),
        }
    }

    fn validate_layered(FramebufferSpecificAttachments { colors, depth_stencil }:
                        FramebufferSpecificAttachments<LayeredAttachment<'a>>)
                        -> Result<ValidatedAttachments<'a>, ValidationError>
    {
        // TODO: make sure that all attachments are layered

        macro_rules! handle_tex {
            ($tex:ident, $dim:ident, $samples:ident, $num_bits:ident) => ({
                $num_bits = Some($tex.get_texture().get_internal_format()
                                     .map(|f| f.get_total_bits()).ok().unwrap_or(24) as u16);     // TODO: how to handle this?
                handle_tex!($tex, $dim, $samples)
            });

            ($tex:ident, $dim:ident, $samples:ident) => ({
                // TODO: check that internal format is renderable
                let context = $tex.get_texture().get_context();

                match &mut $samples {
                    &mut Some(samples) => {
                        if samples != $tex.get_samples().unwrap_or(0) {
                            return Err(ValidationError::SamplesCountMismatch);
                        }
                    },
                    s @ &mut None => {
                        *s = Some($tex.get_samples().unwrap_or(0));
                    }
                }

                match &mut $dim {
                    &mut Some((ref mut w, ref mut h)) => {
                        let height = $tex.get_height().unwrap_or(1);
                        if *w != $tex.get_width() || *h != height {
                            *w = cmp::min(*w, $tex.get_width());
                            *h = cmp::min(*h, height);

                            // checking that multiple different sizes is supported by the backend
                            if !is_dimensions_mismatch_supported(context) {
                                return Err(ValidationError::DimensionsMismatchNotSupported);
                            }
                        }
                    },

                    dim @ &mut None => {
                        *dim = Some(($tex.get_width(), $tex.get_height().unwrap_or(1)));
                    },
                }

                RawAttachment::Texture {
                    texture: $tex.get_texture().get_id(),
                    bind_point: $tex.get_texture().get_bind_point(),
                    layer: None,
                    level: $tex.get_level(),
                    cubemap_layer: None,
                }
            });
        }

        let mut raw_attachments = RawAttachments {
            color: Vec::with_capacity(colors.len()),
            depth: None,
            stencil: None,
            depth_stencil: None,
        };

        let mut dimensions = None;
        let mut depth_bits = None;
        let mut stencil_bits = None;
        let mut samples = None;     // contains `0` if not multisampling and `None` if unknown

        for &(index, LayeredAttachment(ref attachment)) in colors.iter() {
            raw_attachments.color.push((index, handle_tex!(attachment, dimensions, samples)));
        }

        match depth_stencil {
            DepthStencilAttachments::None => (),
            DepthStencilAttachments::DepthAttachment(LayeredAttachment(ref d)) => {
                raw_attachments.depth = Some(handle_tex!(d, dimensions, samples, depth_bits));
            },
            DepthStencilAttachments::StencilAttachment(LayeredAttachment(ref s)) => {
                raw_attachments.stencil = Some(handle_tex!(s, dimensions, samples, stencil_bits));
            },
            DepthStencilAttachments::DepthAndStencilAttachments(LayeredAttachment(ref d),
                                                                 LayeredAttachment(ref s))
            => {
                raw_attachments.depth = Some(handle_tex!(d, dimensions, samples, depth_bits));
                raw_attachments.stencil = Some(handle_tex!(s, dimensions, samples, stencil_bits));
            },
            DepthStencilAttachments::DepthStencilAttachment(LayeredAttachment(ref ds)) => {
                // FIXME: bits count
                raw_attachments.depth_stencil = Some(handle_tex!(ds, dimensions, samples));
            },
        }

        let dimensions = if let Some(dimensions) = dimensions {
            dimensions
        } else {
            // TODO: handle this
            return Err(ValidationError::EmptyFramebufferObjectsNotSupported);
        };

        Ok(ValidatedAttachments {
            raw: raw_attachments,
            dimensions: dimensions,
            layers: None,       // FIXME: count layers
            depth_buffer_bits: depth_bits,
            stencil_buffer_bits: stencil_bits,
            marker: PhantomData,
        })
    }

    fn validate_regular(FramebufferSpecificAttachments { colors, depth_stencil }:
                        FramebufferSpecificAttachments<RegularAttachment<'a>>)
                        -> Result<ValidatedAttachments<'a>, ValidationError>
    {
        macro_rules! handle_tex {
            ($tex:ident, $dim:ident, $samples:ident, $num_bits:ident) => ({
                $num_bits = Some($tex.get_texture().get_internal_format()
                                     .map(|f| f.get_total_bits()).ok().unwrap_or(24) as u16);     // TODO: how to handle this?
                handle_tex!($tex, $dim, $samples)
            });

            ($tex:ident, $dim:ident, $samples:ident) => ({
                // TODO: check that internal format is renderable
                let context = $tex.get_texture().get_context();

                match &mut $samples {
                    &mut Some(samples) => {
                        if samples != $tex.get_samples().unwrap_or(0) {
                            return Err(ValidationError::SamplesCountMismatch);
                        }
                    },
                    s @ &mut None => {
                        *s = Some($tex.get_samples().unwrap_or(0));
                    }
                }

                match &mut $dim {
                    &mut Some((ref mut w, ref mut h)) => {
                        let height = $tex.get_height().unwrap_or(1);
                        if *w != $tex.get_width() || *h != height {
                            *w = cmp::min(*w, $tex.get_width());
                            *h = cmp::min(*h, height);

                            // checking that multiple different sizes is supported by the backend
                            if !is_dimensions_mismatch_supported(context) {
                                return Err(ValidationError::DimensionsMismatchNotSupported);
                            }
                        }
                    },

                    dim @ &mut None => {
                        *dim = Some(($tex.get_width(), $tex.get_height().unwrap_or(1)));
                    },
                }

                RawAttachment::Texture {
                    texture: $tex.get_texture().get_id(),
                    bind_point: $tex.get_texture().get_bind_point(),
                    layer: Some($tex.get_layer()),
                    level: $tex.get_level(),
                    cubemap_layer: $tex.get_cubemap_layer(),
                }
            });
        }

        macro_rules! handle_rb {
            ($rb:ident, $dim:ident, $samples:ident, $num_bits:ident) => ({
                $num_bits = Some(24);       // FIXME: totally arbitrary
                handle_rb!($rb, $dim, $samples)
            });

            ($rb:ident, $dim:ident, $samples:ident) => ({
                // TODO: check that internal format is renderable
                let context = $rb.get_context();
                let dimensions = $rb.get_dimensions();

                match &mut $samples {
                    &mut Some(samples) => {
                        if samples != $rb.get_samples().unwrap_or(0) {
                            return Err(ValidationError::SamplesCountMismatch);
                        }
                    },
                    s @ &mut None => {
                        *s = Some($rb.get_samples().unwrap_or(0));
                    }
                }

                match &mut $dim {
                    &mut Some((ref mut w, ref mut h)) => {
                        if *w != dimensions.0 || *h != dimensions.1 {
                            *w = cmp::min(*w, dimensions.0);
                            *h = cmp::min(*h, dimensions.1);

                            // checking that multiple different sizes is supported by the backend
                            if !is_dimensions_mismatch_supported(context) {
                                return Err(ValidationError::DimensionsMismatchNotSupported);
                            }
                        }
                    },

                    dim @ &mut None => {
                        *dim = Some((dimensions.0, dimensions.1));
                    },
                }

                RawAttachment::RenderBuffer($rb.get_id())
            });
        }

        macro_rules! handle_atch {
            ($atch:ident, $($t:tt)*) => (
                match $atch {
                    &RegularAttachment::Texture(ref tex) => handle_tex!(tex, $($t)*),
                    &RegularAttachment::RenderBuffer(ref rb) => handle_rb!(rb, $($t)*),
                }
            );
        }

        let mut raw_attachments = RawAttachments {
            color: Vec::with_capacity(colors.len()),
            depth: None,
            stencil: None,
            depth_stencil: None,
        };

        let mut dimensions = None;
        let mut depth_bits = None;
        let mut stencil_bits = None;
        let mut samples = None;     // contains `0` if not multisampling and `None` if unknown

        for &(index, ref attachment) in colors.iter() {
            raw_attachments.color.push((index, handle_atch!(attachment, dimensions, samples)));
        }

        match depth_stencil {
            DepthStencilAttachments::None => (),
            DepthStencilAttachments::DepthAttachment(ref d) => {
                raw_attachments.depth = Some(handle_atch!(d, dimensions, samples, depth_bits));
            },
            DepthStencilAttachments::StencilAttachment(ref s) => {
                raw_attachments.stencil = Some(handle_atch!(s, dimensions, samples, stencil_bits));
            },
            DepthStencilAttachments::DepthAndStencilAttachments(ref d, ref s) => {
                raw_attachments.depth = Some(handle_atch!(d, dimensions, samples, depth_bits));
                raw_attachments.stencil = Some(handle_atch!(s, dimensions, samples, stencil_bits));
            },
            DepthStencilAttachments::DepthStencilAttachment(ref ds) => {
                // FIXME: bits count
                raw_attachments.depth_stencil = Some(handle_atch!(ds, dimensions, samples));
            },
        }

        let dimensions = if let Some(dimensions) = dimensions {
            dimensions
        } else {
            // TODO: handle this
            return Err(ValidationError::EmptyFramebufferObjectsNotSupported);
        };

        Ok(ValidatedAttachments {
            raw: raw_attachments,
            dimensions: dimensions,
            layers: None,
            depth_buffer_bits: depth_bits,
            stencil_buffer_bits: stencil_bits,
            marker: PhantomData,
        })
    }
}

/// Represents attachments that have been validated and are usable.
#[derive(Clone)]
pub struct ValidatedAttachments<'a> {
    raw: RawAttachments,
    dimensions: (u32, u32),
    layers: Option<u32>,
    depth_buffer_bits: Option<u16>,
    stencil_buffer_bits: Option<u16>,
    marker: PhantomData<&'a ()>,
}

impl<'a> ValidatedAttachments<'a> {
    /// Returns `true` if the framebuffer is layered.
    #[inline]
    pub fn is_layered(&self) -> bool {
        self.layers.is_some()
    }

    /// Returns the dimensions that the framebuffer will have if you use these attachments.
    #[inline]
    pub fn get_dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    /// Returns the number of bits of precision of the depth buffer, or `None` if there is no
    /// depth buffer. Also works for depth-stencil buffers.
    #[inline]
    pub fn get_depth_buffer_bits(&self) -> Option<u16> {
        self.depth_buffer_bits
    }

    /// Returns the number of bits of precision of the stencil buffer, or `None` if there is no
    /// stencil buffer. Also works for depth-stencil buffers.
    #[inline]
    pub fn get_stencil_buffer_bits(&self) -> Option<u16> {
        self.stencil_buffer_bits
    }
}

/// An error that can happen while validating attachments.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    EmptyFramebufferObjectsNotSupported,

    /// The backend doesn't support attachments with various dimensions.
    ///
    /// Note that almost all OpenGL implementations support attachments with various dimensions.
    /// Only very old versions don't.
    DimensionsMismatchNotSupported,

    /// All attachments must have the same number of samples.
    SamplesCountMismatch,

    /// Backends only support a certain number of color attachments.
    TooManyColorAttachments {
        /// Maximum number of attachments.
        maximum: usize,
        /// Number of attachments that were given.
        obtained: usize,
    },
}

/// Data structure stored in the hashmap.
///
/// These attachments are guaranteed to be valid.
#[derive(Hash, Clone, Eq, PartialEq)]
struct RawAttachments {
    color: Vec<(u32, RawAttachment)>,
    depth: Option<RawAttachment>,
    stencil: Option<RawAttachment>,
    depth_stencil: Option<RawAttachment>,
}

/// Single attachment of `RawAttachments`.
#[derive(Hash, Copy, Clone, Eq, PartialEq)]
enum RawAttachment {
    /// A texture.
    Texture {
        // a GLenum like `TEXTURE_2D`, `TEXTURE_3D`, etc.
        bind_point: gl::types::GLenum,      // TODO: Dimensions instead
        // id of the texture
        texture: gl::types::GLuint,
        // if `Some`, use a regular attachment ; if `None`, use a layered attachment
        // if `None`, the texture **must** be an array, cubemap, or texture 3d
        layer: Option<u32>,
        // mipmap level
        level: u32,
        // layer of the cubemap, if this is a cubemap
        cubemap_layer: Option<CubeLayer>,
    },

    /// A renderbuffer with its ID.
    RenderBuffer(gl::types::GLuint),
}

/// Manages all the framebuffer objects.
///
/// `cleanup` **must** be called when destroying the container, otherwise `Drop` will panic.
pub struct FramebuffersContainer {
    framebuffers: RefCell<HashMap<RawAttachments, FrameBufferObject>>,
}

impl FramebuffersContainer {
    /// Initializes the container.
    #[inline]
    pub fn new() -> FramebuffersContainer {
        FramebuffersContainer {
            framebuffers: RefCell::new(HashMap::new()),
        }
    }

    /// Destroys all framebuffer objects. This is used when using a new context for example.
    pub fn purge_all(ctxt: &mut CommandContext) {
        let mut other = HashMap::new();
        mem::swap(&mut *ctxt.framebuffer_objects.framebuffers.borrow_mut(), &mut other);

        for (_, obj) in other.into_iter() {
            obj.destroy(ctxt);
        }
    }

    /// Destroys all framebuffer objects that contain a precise texture.
    #[inline]
    pub fn purge_texture(ctxt: &mut CommandContext, texture: gl::types::GLuint) {
        FramebuffersContainer::purge_if(ctxt, |a| {
            match a {
                &RawAttachment::Texture { texture: id, .. } if id == texture => true,
                _ => false 
            }
        });
    }

    /// Destroys all framebuffer objects that contain a precise renderbuffer.
    #[inline]
    pub fn purge_renderbuffer(ctxt: &mut CommandContext, renderbuffer: gl::types::GLuint) {
        FramebuffersContainer::purge_if(ctxt, |a| a == &RawAttachment::RenderBuffer(renderbuffer));
    }

    /// Destroys all framebuffer objects that match a certain condition.
    fn purge_if<F>(mut ctxt: &mut CommandContext, condition: F)
                   where F: Fn(&RawAttachment) -> bool
    {
        let mut framebuffers = ctxt.framebuffer_objects.framebuffers.borrow_mut();

        let mut attachments = Vec::with_capacity(0);
        for (key, _) in framebuffers.iter() {
            if key.color.iter().find(|&&(_, ref id)| condition(id)).is_some() {
                attachments.push(key.clone());
                continue;
            }

            if let Some(ref atch) = key.depth {
                if condition(atch) {
                    attachments.push(key.clone());
                    continue;
                }
            }

            if let Some(ref atch) = key.stencil {
                if condition(atch) {
                    attachments.push(key.clone());
                    continue;
                }
            }

            if let Some(ref atch) = key.depth_stencil {
                if condition(atch) {
                    attachments.push(key.clone());
                    continue;
                }
            }
        }

        for atch in attachments.into_iter() {
            framebuffers.remove(&atch).unwrap().destroy(ctxt);
        }
    }

    /// Destroys all framebuffer objects.
    ///
    /// This is very similar to `purge_all`, but optimized for when the container will soon
    /// be destroyed.
    pub fn cleanup(ctxt: &mut CommandContext) {
        let mut other = HashMap::with_capacity(0);
        mem::swap(&mut *ctxt.framebuffer_objects.framebuffers.borrow_mut(), &mut other);

        for (_, obj) in other.into_iter() {
            obj.destroy(ctxt);
        }
    }

    ///
    /// # Unsafety
    ///
    /// After calling this function, you **must** make sure to call `purge_texture`
    /// and/or `purge_renderbuffer` when one of the attachment is destroyed.
    #[inline]
    pub fn get_framebuffer_for_drawing(ctxt: &mut CommandContext,
                                       attachments: Option<&ValidatedAttachments>)
                                       -> gl::types::GLuint
    {
        if let Some(attachments) = attachments {
            FramebuffersContainer::get_framebuffer(ctxt, attachments)
        } else {
            0
        }
    }

    /// Binds the default framebuffer to `GL_READ_FRAMEBUFFER` or `GL_FRAMEBUFFER` so that it
    /// becomes the target of `glReadPixels`, `glCopyTexImage2D`, etc.
    // TODO: use an enum for the read buffer instead
    #[inline]
    pub fn bind_default_framebuffer_for_reading(ctxt: &mut CommandContext,
                                                read_buffer: gl::types::GLenum)
    {
        unsafe { bind_framebuffer(ctxt, 0, false, true) };
        unsafe { ctxt.gl.ReadBuffer(read_buffer) };     // TODO: cache
    }

    /// Binds a framebuffer to `GL_READ_FRAMEBUFFER` or `GL_FRAMEBUFFER` so that it becomes the
    /// target of `glReadPixels`, `glCopyTexImage2D`, etc.
    ///
    /// # Unsafety
    ///
    /// After calling this function, you **must** make sure to call `purge_texture`
    /// and/or `purge_renderbuffer` when one of the attachment is destroyed.
    pub unsafe fn bind_framebuffer_for_reading(ctxt: &mut CommandContext, attachment: &RegularAttachment) {
        // TODO: restore this optimisation
        /*for (attachments, fbo) in ctxt.framebuffer_objects.framebuffers.borrow_mut().iter() {
            for &(key, ref atc) in attachments.color.iter() {
                if atc == attachment {
                    return (fbo.get_id(), gl::COLOR_ATTACHMENT0 + key);
                }
            }
        }*/

        let attachments = FramebufferAttachments::Regular(FramebufferSpecificAttachments {
            colors: { let mut v = SmallVec::new(); v.push((0, attachment.clone())); v },
            depth_stencil: DepthStencilAttachments::None,
        }).validate().unwrap();

        let framebuffer = FramebuffersContainer::get_framebuffer_for_drawing(ctxt, Some(&attachments));
        bind_framebuffer(ctxt, framebuffer, false, true);
        ctxt.gl.ReadBuffer(gl::COLOR_ATTACHMENT0);     // TODO: cache
    }

    ///
    /// # Unsafety
    ///
    /// After calling this function, you **must** make sure to call `purge_texture`
    /// and/or `purge_renderbuffer` when one of the attachment is destroyed.
    fn get_framebuffer(ctxt: &mut CommandContext, attachments: &ValidatedAttachments)
                       -> gl::types::GLuint
    {
        // TODO: use entries API
        let mut framebuffers = ctxt.framebuffer_objects.framebuffers.borrow_mut();
        if let Some(value) = framebuffers.get(&attachments.raw) {
            return value.id;
        }

        let new_fbo = FrameBufferObject::new(ctxt, &attachments.raw);
        let new_fbo_id = new_fbo.id.clone();
        framebuffers.insert(attachments.raw.clone(), new_fbo);
        new_fbo_id
    }
}

impl Drop for FramebuffersContainer {
    #[inline]
    fn drop(&mut self) {
        if self.framebuffers.borrow().len() != 0 {
            panic!()
        }
    }
}

/// A framebuffer object.
struct FrameBufferObject {
    id: gl::types::GLuint,
    current_read_buffer: gl::types::GLenum,
}

impl FrameBufferObject {
    /// Builds a new FBO.
    ///
    /// # Panic
    ///
    /// Panicks if anything wrong or not supported is detected with the raw attachments.
    ///
    fn new(mut ctxt: &mut CommandContext, attachments: &RawAttachments) -> FrameBufferObject {
        if attachments.color.len() > ctxt.capabilities.max_draw_buffers as usize {
            panic!("Trying to attach {} color buffers, but the hardware only supports {}",
                   attachments.color.len(), ctxt.capabilities.max_draw_buffers);
        }

        // building the FBO
        let id = unsafe {
            let mut id = mem::uninitialized();

            if ctxt.version >= &Version(Api::Gl, 4, 5) ||
                ctxt.extensions.gl_arb_direct_state_access
            {
                ctxt.gl.CreateFramebuffers(1, &mut id);

            } else if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                      ctxt.version >= &Version(Api::GlEs, 2, 0) ||
                      ctxt.extensions.gl_arb_framebuffer_object
            {
                ctxt.gl.GenFramebuffers(1, &mut id);
                bind_framebuffer(&mut ctxt, id, true, false);

            } else if ctxt.extensions.gl_ext_framebuffer_object {
                ctxt.gl.GenFramebuffersEXT(1, &mut id);
                bind_framebuffer(&mut ctxt, id, true, false);

            } else {
                // glium doesn't allow creating contexts that don't support FBOs
                unreachable!();
            }

            id
        };

        // attaching the attachments, and building the list of enums to pass to `glDrawBuffers`
        let mut raw_attachments = Vec::with_capacity(attachments.color.len());
        for &(slot, atchmnt) in attachments.color.iter() {
            if slot >= ctxt.capabilities.max_color_attachments as u32 {
                panic!("Trying to attach a color buffer to slot {}, but the hardware only supports {} bind points",
                    slot, ctxt.capabilities.max_color_attachments);
            }
            unsafe { attach(&mut ctxt, gl::COLOR_ATTACHMENT0 + slot as u32, id, atchmnt) };
            raw_attachments.push(gl::COLOR_ATTACHMENT0 + slot as gl::types::GLenum);
        }
        if let Some(depth) = attachments.depth {
            unsafe { attach(&mut ctxt, gl::DEPTH_ATTACHMENT, id, depth) };
        }
        if let Some(stencil) = attachments.stencil {
            unsafe { attach(&mut ctxt, gl::STENCIL_ATTACHMENT, id, stencil) };
        }
        if let Some(depth_stencil) = attachments.depth_stencil {
            unsafe { attach(&mut ctxt, gl::DEPTH_STENCIL_ATTACHMENT, id, depth_stencil) };
        }

        // calling `glDrawBuffers` if necessary
        if raw_attachments != &[gl::COLOR_ATTACHMENT0] {
            if ctxt.version >= &Version(Api::Gl, 4, 5) ||
               ctxt.extensions.gl_arb_direct_state_access
            {
                unsafe {
                    ctxt.gl.NamedFramebufferDrawBuffers(id, raw_attachments.len()
                                                        as gl::types::GLsizei,
                                                        raw_attachments.as_ptr());
                }

            } else if ctxt.version >= &Version(Api::Gl, 2, 0) ||
                      ctxt.version >= &Version(Api::GlEs, 3, 0)
            {
                unsafe {
                    bind_framebuffer(&mut ctxt, id, true, false);
                    ctxt.gl.DrawBuffers(raw_attachments.len() as gl::types::GLsizei,
                                        raw_attachments.as_ptr());
                }

            } else if ctxt.extensions.gl_arb_draw_buffers {
                unsafe {
                    bind_framebuffer(&mut ctxt, id, true, false);
                    ctxt.gl.DrawBuffersARB(raw_attachments.len() as gl::types::GLsizei,
                                           raw_attachments.as_ptr());
                }

            } else if ctxt.extensions.gl_ati_draw_buffers {
                unsafe {
                    bind_framebuffer(&mut ctxt, id, true, false);
                    ctxt.gl.DrawBuffersATI(raw_attachments.len() as gl::types::GLsizei,
                                           raw_attachments.as_ptr());
                }

            } else {
                // OpenGL ES 2 and OpenGL 1 don't support calling `glDrawBuffers`
                panic!("Using more than one attachment is not supported by the backend");
            }
        }


        FrameBufferObject {
            id: id,
            current_read_buffer: gl::BACK,
        }
    }

    /// Destroys the FBO. Must be called, or things will leak.
    fn destroy(self, mut ctxt: &mut CommandContext) {
        // unbinding framebuffer
        if ctxt.state.draw_framebuffer == self.id {
            ctxt.state.draw_framebuffer = 0;
        }

        if ctxt.state.read_framebuffer == self.id {
            ctxt.state.read_framebuffer = 0;
        }

        // deleting
        if ctxt.version >= &Version(Api::Gl, 3, 0) ||
            ctxt.version >= &Version(Api::GlEs, 2, 0) ||
            ctxt.extensions.gl_arb_framebuffer_object
        {
            unsafe { ctxt.gl.DeleteFramebuffers(1, [ self.id ].as_ptr()) };
        } else if ctxt.extensions.gl_ext_framebuffer_object {
            unsafe { ctxt.gl.DeleteFramebuffersEXT(1, [ self.id ].as_ptr()) };
        } else {
            unreachable!();
        }
    }
}

impl GlObject for FrameBufferObject {
    type Id = gl::types::GLuint;
    
    #[inline]
    fn get_id(&self) -> gl::types::GLuint {
        self.id
    }
}

/// Binds a framebuffer object, either for drawing, reading, or both.
///
/// # Safety
///
/// The id of the FBO must be valid.
///
pub unsafe fn bind_framebuffer(ctxt: &mut CommandContext, fbo_id: gl::types::GLuint,
                               draw: bool, read: bool)
{
    if draw && read {
        if ctxt.state.draw_framebuffer != fbo_id || ctxt.state.read_framebuffer != fbo_id {
            if ctxt.version >= &Version(Api::Gl, 3, 0) ||
               ctxt.version >= &Version(Api::GlEs, 2, 0) ||
               ctxt.extensions.gl_arb_framebuffer_object
            {
                ctxt.gl.BindFramebuffer(gl::FRAMEBUFFER, fbo_id);
                ctxt.state.draw_framebuffer = fbo_id;
                ctxt.state.read_framebuffer = fbo_id;
            } else if ctxt.extensions.gl_ext_framebuffer_object {
                ctxt.gl.BindFramebufferEXT(gl::FRAMEBUFFER_EXT, fbo_id);
                ctxt.state.draw_framebuffer = fbo_id;
                ctxt.state.read_framebuffer = fbo_id;
            } else {
                unreachable!();
            }
        }


    } else {

        if draw && ctxt.state.draw_framebuffer != fbo_id {
            if ctxt.version >= &Version(Api::Gl, 3, 0) ||
               ctxt.extensions.gl_arb_framebuffer_object
            {
                ctxt.gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, fbo_id);
                ctxt.state.draw_framebuffer = fbo_id;
            } else if ctxt.version >= &Version(Api::GlEs, 2, 0) {
                ctxt.gl.BindFramebuffer(gl::FRAMEBUFFER, fbo_id);
                ctxt.state.draw_framebuffer = fbo_id;
                ctxt.state.read_framebuffer = fbo_id;
            } else if ctxt.extensions.gl_ext_framebuffer_object {
                ctxt.gl.BindFramebufferEXT(gl::FRAMEBUFFER_EXT, fbo_id);
                ctxt.state.draw_framebuffer = fbo_id;
                ctxt.state.read_framebuffer = fbo_id;
            } else {
                unreachable!();
            }
        }

        if read && ctxt.state.read_framebuffer != fbo_id {
            if ctxt.version >= &Version(Api::Gl, 3, 0) ||
               ctxt.extensions.gl_arb_framebuffer_object
            {
                ctxt.gl.BindFramebuffer(gl::READ_FRAMEBUFFER, fbo_id);
                ctxt.state.read_framebuffer = fbo_id;
            } else if ctxt.version >= &Version(Api::GlEs, 2, 0) {
                ctxt.gl.BindFramebuffer(gl::FRAMEBUFFER, fbo_id);
                ctxt.state.draw_framebuffer = fbo_id;
                ctxt.state.read_framebuffer = fbo_id;
            } else if ctxt.extensions.gl_ext_framebuffer_object {
                ctxt.gl.BindFramebufferEXT(gl::FRAMEBUFFER_EXT, fbo_id);
                ctxt.state.draw_framebuffer = fbo_id;
                ctxt.state.read_framebuffer = fbo_id;
            } else {
                unreachable!();
            }
        }

    }
}

/// Attaches something to a framebuffer object.
///
/// # Panic
///
/// - Panicks if `layer` is `None` and layered attachments are not supported.
/// - Panicks if `layer` is `None` and the texture is not an array or a 3D texture.
/// - Panicks if the texture is an array and attaching an array is not supported.
///
/// # Safety
///
/// All parameters must be valid.
///
unsafe fn attach(ctxt: &mut CommandContext, slot: gl::types::GLenum,
                 id: gl::types::GLuint, attachment: RawAttachment)
{
    match attachment {
        RawAttachment::Texture { texture: tex_id, level, layer, bind_point, cubemap_layer } => {
            match bind_point {
                // these textures can't be layered
                gl::TEXTURE_2D | gl::TEXTURE_2D_MULTISAMPLE | gl::TEXTURE_1D |
                gl::TEXTURE_RECTANGLE =>
                {
                    assert_eq!(layer, Some(0));
                    debug_assert!(cubemap_layer.is_none());

                    if ctxt.version >= &Version(Api::Gl, 4, 5) ||
                       ctxt.extensions.gl_arb_direct_state_access
                    {
                        ctxt.gl.NamedFramebufferTexture(id, slot, tex_id,
                                                        level as gl::types::GLint);

                    } else if ctxt.extensions.gl_ext_direct_state_access &&
                              ctxt.extensions.gl_ext_geometry_shader4
                    {
                        ctxt.gl.NamedFramebufferTextureEXT(id, slot, tex_id,
                                                           level as gl::types::GLint);

                    } else if ctxt.version >= &Version(Api::Gl, 3, 2) {
                        bind_framebuffer(ctxt, id, true, false);
                        ctxt.gl.FramebufferTexture(gl::DRAW_FRAMEBUFFER,
                                                   slot, tex_id, level as gl::types::GLint);

                    } else if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                              ctxt.extensions.gl_arb_framebuffer_object
                    {
                        bind_framebuffer(ctxt, id, true, false);

                        match bind_point {
                            gl::TEXTURE_1D | gl::TEXTURE_RECTANGLE => {
                                ctxt.gl.FramebufferTexture1D(gl::DRAW_FRAMEBUFFER,
                                                             slot, bind_point, tex_id,
                                                             level as gl::types::GLint);
                            },
                            gl::TEXTURE_2D | gl::TEXTURE_2D_MULTISAMPLE => {
                                ctxt.gl.FramebufferTexture2D(gl::DRAW_FRAMEBUFFER,
                                                             slot, bind_point, tex_id,
                                                             level as gl::types::GLint);
                            },
                            _ => unreachable!()
                        }

                    } else if ctxt.version >= &Version(Api::GlEs, 2, 0) {
                        bind_framebuffer(ctxt, id, true, true);
                        assert!(bind_point == gl::TEXTURE_2D);
                        ctxt.gl.FramebufferTexture2D(gl::FRAMEBUFFER, slot, bind_point, tex_id,
                                                     level as gl::types::GLint);

                    } else if ctxt.extensions.gl_ext_framebuffer_object {
                        bind_framebuffer(ctxt, id, true, true);

                        match bind_point {
                            gl::TEXTURE_1D | gl::TEXTURE_RECTANGLE => {
                                ctxt.gl.FramebufferTexture1DEXT(gl::FRAMEBUFFER_EXT,
                                                                slot, bind_point, tex_id,
                                                                level as gl::types::GLint);
                            },
                            gl::TEXTURE_2D | gl::TEXTURE_2D_MULTISAMPLE => {
                                ctxt.gl.FramebufferTexture2DEXT(gl::FRAMEBUFFER_EXT,
                                                                slot, bind_point, tex_id,
                                                                level as gl::types::GLint);
                            },
                            _ => unreachable!()
                        }

                    } else {
                        // it's not possible to create an OpenGL context that doesn't support FBOs
                        unreachable!();
                    }
                },

                // non-layered attachments
                gl::TEXTURE_1D_ARRAY | gl::TEXTURE_2D_ARRAY | gl::TEXTURE_2D_MULTISAMPLE_ARRAY |
                gl::TEXTURE_3D | gl::TEXTURE_CUBE_MAP_ARRAY if layer.is_some() =>
                {
                    let layer = if bind_point == gl::TEXTURE_CUBE_MAP_ARRAY {
                        layer.unwrap() * 6 + cubemap_layer.unwrap().get_layer_index()
                                                                               as gl::types::GLenum
                    } else {
                        layer.unwrap()
                    };

                    if ctxt.version >= &Version(Api::Gl, 4, 5) ||
                       ctxt.extensions.gl_arb_direct_state_access
                    {
                        ctxt.gl.NamedFramebufferTextureLayer(id, slot, tex_id,
                                                             level as gl::types::GLint,
                                                             layer as gl::types::GLint);

                    } else if ctxt.extensions.gl_ext_direct_state_access &&
                              ctxt.extensions.gl_ext_geometry_shader4
                    {
                        ctxt.gl.NamedFramebufferTextureLayerEXT(id, slot, tex_id,
                                                                level as gl::types::GLint,
                                                                layer as gl::types::GLint);

                    } else if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                              ctxt.extensions.gl_arb_framebuffer_object
                    {
                        bind_framebuffer(ctxt, id, true, false);

                        match bind_point {
                            gl::TEXTURE_1D_ARRAY | gl::TEXTURE_2D_ARRAY |
                            gl::TEXTURE_2D_MULTISAMPLE_ARRAY => {
                                ctxt.gl.FramebufferTextureLayer(gl::DRAW_FRAMEBUFFER,
                                                                slot, tex_id,
                                                                level as gl::types::GLint,
                                                                layer as gl::types::GLint);

                            },

                            gl::TEXTURE_3D => {
                                ctxt.gl.FramebufferTexture3D(gl::DRAW_FRAMEBUFFER,
                                                             slot, bind_point, tex_id,
                                                             level as gl::types::GLint,
                                                             layer as gl::types::GLint);
                            },

                            _ => unreachable!()
                        }

                    } else if ctxt.extensions.gl_ext_framebuffer_object &&
                              bind_point == gl::TEXTURE_3D
                    {
                        bind_framebuffer(ctxt, id, true, true);
                        ctxt.gl.FramebufferTexture3DEXT(gl::FRAMEBUFFER_EXT,
                                                        slot, bind_point, tex_id,
                                                        level as gl::types::GLint,
                                                        layer as gl::types::GLint);

                    } else if ctxt.extensions.gl_ext_texture_array &&
                              bind_point == gl::TEXTURE_1D_ARRAY ||
                              bind_point == gl::TEXTURE_2D_ARRAY ||
                              bind_point == gl::TEXTURE_2D_MULTISAMPLE_ARRAY
                    {
                        bind_framebuffer(ctxt, id, true, false);
                        ctxt.gl.FramebufferTextureLayerEXT(gl::DRAW_FRAMEBUFFER,
                                                           slot, tex_id,
                                                           level as gl::types::GLint,
                                                           layer as gl::types::GLint);

                    } else {
                        panic!("Attaching a texture array is not supported");
                    }
                },

                // layered attachments
                gl::TEXTURE_1D_ARRAY | gl::TEXTURE_2D_ARRAY | gl::TEXTURE_2D_MULTISAMPLE_ARRAY |
                gl::TEXTURE_3D | gl::TEXTURE_CUBE_MAP_ARRAY if layer.is_none() =>
                {
                    if ctxt.version >= &Version(Api::Gl, 4, 5) ||
                       ctxt.extensions.gl_arb_direct_state_access
                    {
                        ctxt.gl.NamedFramebufferTexture(id, slot, tex_id,
                                                        level as gl::types::GLint);

                    } else if ctxt.extensions.gl_ext_direct_state_access &&
                              ctxt.extensions.gl_ext_geometry_shader4
                    {
                        ctxt.gl.NamedFramebufferTextureEXT(id, slot, tex_id,
                                                           level as gl::types::GLint);

                    } else if ctxt.version >= &Version(Api::Gl, 3, 2) {
                        bind_framebuffer(ctxt, id, true, false);
                        ctxt.gl.FramebufferTexture(gl::DRAW_FRAMEBUFFER,
                                                   slot, tex_id, level as gl::types::GLint);

                    } else {
                        // note that this should have been detected earlier
                        panic!("Layered framebuffers are not supported");
                    }
                },

                // non-layered cubemaps
                gl::TEXTURE_CUBE_MAP if layer.is_some() => {
                    let bind_point = gl::TEXTURE_CUBE_MAP_POSITIVE_X +
                                    cubemap_layer.unwrap().get_layer_index() as gl::types::GLenum;

                    if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                              ctxt.extensions.gl_arb_framebuffer_object
                    {
                        bind_framebuffer(ctxt, id, true, false);
                        ctxt.gl.FramebufferTexture2D(gl::DRAW_FRAMEBUFFER,
                                                     slot, bind_point, tex_id,
                                                     level as gl::types::GLint);

                    } else if ctxt.version >= &Version(Api::GlEs, 2, 0) {
                        bind_framebuffer(ctxt, id, true, true);
                        ctxt.gl.FramebufferTexture2D(gl::FRAMEBUFFER, slot, bind_point, tex_id,
                                                     level as gl::types::GLint);

                    } else if ctxt.extensions.gl_ext_framebuffer_object {
                        bind_framebuffer(ctxt, id, true, true);
                        ctxt.gl.FramebufferTexture2DEXT(gl::FRAMEBUFFER_EXT,
                                                        slot, bind_point, tex_id,
                                                        level as gl::types::GLint);

                    } else {
                        // it's not possible to create an OpenGL context that doesn't support FBOs
                        unreachable!();
                    }
                },

                // layered cubemaps
                gl::TEXTURE_CUBE_MAP if layer.is_none() => {
                    if ctxt.version >= &Version(Api::Gl, 4, 5) ||
                       ctxt.extensions.gl_arb_direct_state_access
                    {
                        ctxt.gl.NamedFramebufferTexture(id, slot, tex_id,
                                                        level as gl::types::GLint);

                    } else if ctxt.extensions.gl_ext_direct_state_access &&
                              ctxt.extensions.gl_ext_geometry_shader4
                    {
                        ctxt.gl.NamedFramebufferTextureEXT(id, slot, tex_id,
                                                           level as gl::types::GLint);

                    } else if ctxt.version >= &Version(Api::Gl, 3, 2) {
                        bind_framebuffer(ctxt, id, true, false);
                        ctxt.gl.FramebufferTexture(gl::DRAW_FRAMEBUFFER,
                                                   slot, tex_id, level as gl::types::GLint);

                    } else {
                        // note that this should have been detected earlier
                        panic!("Layered framebuffers are not supported");
                    }
                },

                _ => unreachable!()
            }
        },

        // renderbuffers are straight-forward
        RawAttachment::RenderBuffer(renderbuffer) => {
            if ctxt.version >= &Version(Api::Gl, 4, 5) ||
               ctxt.extensions.gl_arb_direct_state_access
            {
                ctxt.gl.NamedFramebufferRenderbuffer(id, slot, gl::RENDERBUFFER, renderbuffer);

            } else if ctxt.extensions.gl_ext_direct_state_access &&
                      ctxt.extensions.gl_ext_geometry_shader4
            {
                ctxt.gl.NamedFramebufferRenderbufferEXT(id, slot, gl::RENDERBUFFER, renderbuffer);

            } else if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                      ctxt.extensions.gl_arb_framebuffer_object
            {
                bind_framebuffer(ctxt, id, true, false);
                ctxt.gl.FramebufferRenderbuffer(gl::DRAW_FRAMEBUFFER, slot,
                                                gl::RENDERBUFFER, renderbuffer);

            } else if ctxt.version >= &Version(Api::GlEs, 2, 0) {
                bind_framebuffer(ctxt, id, true, true);
                ctxt.gl.FramebufferRenderbuffer(gl::DRAW_FRAMEBUFFER, slot,
                                                gl::RENDERBUFFER, renderbuffer);

            } else if ctxt.extensions.gl_ext_framebuffer_object {
                bind_framebuffer(ctxt, id, true, true);
                ctxt.gl.FramebufferRenderbufferEXT(gl::DRAW_FRAMEBUFFER, slot,
                                                   gl::RENDERBUFFER, renderbuffer);

            } else {
                // it's not possible to create an OpenGL context that doesn't support FBOs
                unreachable!();
            }
        },
    }
}
