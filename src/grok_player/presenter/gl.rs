use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::{ComposedFrame, GetProcAddressFn, RGBA_BYTES_PER_PIXEL};
use super::picture_rectangle;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA: u32 = 0x1908;
const GL_RGBA8: i32 = 0x8058;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_ARRAY_BUFFER: u32 = 0x8892;
const GL_STATIC_DRAW: u32 = 0x88E4;
const GL_FLOAT: u32 = 0x1406;
const GL_TRIANGLE_STRIP: u32 = 0x0005;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_FALSE: u8 = 0;
const GL_TRUE: i32 = 1;
const GL_EXTENSIONS: u32 = 0x1F03;
const GL_NUM_EXTENSIONS: u32 = 0x821D;
const GL_UNPACK_CLIENT_STORAGE_APPLE: u32 = 0x85B2;
const GL_TEXTURE_STORAGE_HINT_APPLE: u32 = 0x85BC;
const GL_STORAGE_SHARED_APPLE: i32 = 0x85BF;

const INFO_LOG_BYTES: i32 = 1024;
const QUAD_VERTICES: i32 = 4;
const POSITION_ATTRIBUTE: u32 = 0;
const TEXTURE_COORDINATE_ATTRIBUTE: u32 = 1;
const COMPONENTS_PER_ATTRIBUTE: i32 = 2;
const FLOATS_PER_VERTEX: i32 = 4;
const PICTURE_TEXTURE_UNIT: i32 = 0;

// flip_y follows mpv: true puts the first texture row, the picture's top, at gl's top
const QUAD: [f32; 16] = [
    -1.0, 1.0, 0.0, 0.0, // top left
    -1.0, -1.0, 0.0, 1.0, // bottom left
    1.0, 1.0, 1.0, 0.0, // top right
    1.0, -1.0, 1.0, 1.0, // bottom right
];

// core profile, all three hosts' contexts accept it
const VERTEX_SHADER: &str = "#version 330 core\n\
layout(location = 0) in vec2 position;\n\
layout(location = 1) in vec2 corner;\n\
uniform int flip_y;\n\
out vec2 texture_coordinate;\n\
void main() {\n\
    texture_coordinate = vec2(corner.x, flip_y != 0 ? corner.y : 1.0 - corner.y);\n\
    gl_Position = vec4(position, 0.0, 1.0);\n\
}\n";

const FRAGMENT_SHADER: &str = "#version 330 core\n\
in vec2 texture_coordinate;\n\
uniform sampler2D picture;\n\
out vec4 fragment;\n\
void main() {\n\
    fragment = vec4(texture(picture, texture_coordinate).rgb, 1.0);\n\
}\n";

type GlGenTextures = unsafe extern "C" fn(i32, *mut u32);
type GlBindTexture = unsafe extern "C" fn(u32, u32);
type GlTexImage2D = unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
type GlTexSubImage2D = unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
type GlTexParameteri = unsafe extern "C" fn(u32, u32, i32);
type GlPixelStorei = unsafe extern "C" fn(u32, i32);
type GlCreateShader = unsafe extern "C" fn(u32) -> u32;
type GlShaderSource = unsafe extern "C" fn(u32, i32, *const *const c_char, *const i32);
type GlCompileShader = unsafe extern "C" fn(u32);
type GlGetShaderiv = unsafe extern "C" fn(u32, u32, *mut i32);
type GlGetShaderInfoLog = unsafe extern "C" fn(u32, i32, *mut i32, *mut c_char);
type GlDeleteShader = unsafe extern "C" fn(u32);
type GlCreateProgram = unsafe extern "C" fn() -> u32;
type GlAttachShader = unsafe extern "C" fn(u32, u32);
type GlLinkProgram = unsafe extern "C" fn(u32);
type GlGetProgramiv = unsafe extern "C" fn(u32, u32, *mut i32);
type GlGetProgramInfoLog = unsafe extern "C" fn(u32, i32, *mut i32, *mut c_char);
type GlUseProgram = unsafe extern "C" fn(u32);
type GlGetUniformLocation = unsafe extern "C" fn(u32, *const c_char) -> i32;
type GlUniform1i = unsafe extern "C" fn(i32, i32);
type GlGenVertexArrays = unsafe extern "C" fn(i32, *mut u32);
type GlBindVertexArray = unsafe extern "C" fn(u32);
type GlGenBuffers = unsafe extern "C" fn(i32, *mut u32);
type GlBindBuffer = unsafe extern "C" fn(u32, u32);
type GlBufferData = unsafe extern "C" fn(u32, isize, *const c_void, u32);
type GlVertexAttribPointer = unsafe extern "C" fn(u32, i32, u32, u8, i32, *const c_void);
type GlEnableVertexAttribArray = unsafe extern "C" fn(u32);
type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
type GlViewport = unsafe extern "C" fn(i32, i32, i32, i32);
type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type GlClear = unsafe extern "C" fn(u32);
type GlDrawArrays = unsafe extern "C" fn(u32, i32, i32);
type GlActiveTexture = unsafe extern "C" fn(u32);
type GlGetIntegerv = unsafe extern "C" fn(u32, *mut i32);
type GlGetStringi = unsafe extern "C" fn(u32, u32) -> *const u8;

struct Entries {
    gen_textures: GlGenTextures,
    bind_texture: GlBindTexture,
    tex_image_2d: GlTexImage2D,
    tex_sub_image_2d: GlTexSubImage2D,
    tex_parameteri: GlTexParameteri,
    pixel_storei: GlPixelStorei,
    create_shader: GlCreateShader,
    shader_source: GlShaderSource,
    compile_shader: GlCompileShader,
    get_shaderiv: GlGetShaderiv,
    get_shader_info_log: GlGetShaderInfoLog,
    delete_shader: GlDeleteShader,
    create_program: GlCreateProgram,
    attach_shader: GlAttachShader,
    link_program: GlLinkProgram,
    get_programiv: GlGetProgramiv,
    get_program_info_log: GlGetProgramInfoLog,
    use_program: GlUseProgram,
    get_uniform_location: GlGetUniformLocation,
    uniform1i: GlUniform1i,
    gen_vertex_arrays: GlGenVertexArrays,
    bind_vertex_array: GlBindVertexArray,
    gen_buffers: GlGenBuffers,
    bind_buffer: GlBindBuffer,
    buffer_data: GlBufferData,
    vertex_attrib_pointer: GlVertexAttribPointer,
    enable_vertex_attrib_array: GlEnableVertexAttribArray,
    bind_framebuffer: GlBindFramebuffer,
    viewport: GlViewport,
    clear_colour: GlClearColor,
    clear: GlClear,
    draw_arrays: GlDrawArrays,
    active_texture: GlActiveTexture,
    get_integerv: GlGetIntegerv,
    get_stringi: GlGetStringi,
}

// every method must run on the thread whose gl context built it
pub(crate) struct GlPresenter {
    entries: Entries,
    program: u32,
    vertex_array: u32,
    texture: u32,
    // a decode scale change reallocates the texture
    texture_size: (u32, u32),
    uploaded_serial: u64,
    picture_uniform: i32,
    flip_y_uniform: i32,
    // GL_APPLE_client_storage: the texture is the decoded buffer, so the Arc
    // has to outlive the draw that samples it
    client_storage: bool,
    backing: Option<Arc<ComposedFrame>>,
}

// fails by name rather than leaving a null that draws nothing
unsafe fn entry<T: Copy>(
    loader: GetProcAddressFn,
    context: *mut c_void,
    name: &str,
) -> Result<T, String> {
    let symbol = CString::new(name).map_err(|_| format!("bad GL entry point name {name}"))?;
    let address = unsafe { loader(context, symbol.as_ptr()) };
    if address.is_null() {
        return Err(format!("this GL context has no {name}"));
    }
    // a gl loader hands back a code address, which is what T is
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&address) })
}

impl GlPresenter {
    pub fn build(loader: GetProcAddressFn, context: *mut c_void) -> Result<Self, String> {
        let entries = unsafe {
            Entries {
                gen_textures: entry(loader, context, "glGenTextures")?,
                bind_texture: entry(loader, context, "glBindTexture")?,
                tex_image_2d: entry(loader, context, "glTexImage2D")?,
                tex_sub_image_2d: entry(loader, context, "glTexSubImage2D")?,
                tex_parameteri: entry(loader, context, "glTexParameteri")?,
                pixel_storei: entry(loader, context, "glPixelStorei")?,
                create_shader: entry(loader, context, "glCreateShader")?,
                shader_source: entry(loader, context, "glShaderSource")?,
                compile_shader: entry(loader, context, "glCompileShader")?,
                get_shaderiv: entry(loader, context, "glGetShaderiv")?,
                get_shader_info_log: entry(loader, context, "glGetShaderInfoLog")?,
                delete_shader: entry(loader, context, "glDeleteShader")?,
                create_program: entry(loader, context, "glCreateProgram")?,
                attach_shader: entry(loader, context, "glAttachShader")?,
                link_program: entry(loader, context, "glLinkProgram")?,
                get_programiv: entry(loader, context, "glGetProgramiv")?,
                get_program_info_log: entry(loader, context, "glGetProgramInfoLog")?,
                use_program: entry(loader, context, "glUseProgram")?,
                get_uniform_location: entry(loader, context, "glGetUniformLocation")?,
                uniform1i: entry(loader, context, "glUniform1i")?,
                gen_vertex_arrays: entry(loader, context, "glGenVertexArrays")?,
                bind_vertex_array: entry(loader, context, "glBindVertexArray")?,
                gen_buffers: entry(loader, context, "glGenBuffers")?,
                bind_buffer: entry(loader, context, "glBindBuffer")?,
                buffer_data: entry(loader, context, "glBufferData")?,
                vertex_attrib_pointer: entry(loader, context, "glVertexAttribPointer")?,
                enable_vertex_attrib_array: entry(loader, context, "glEnableVertexAttribArray")?,
                bind_framebuffer: entry(loader, context, "glBindFramebuffer")?,
                viewport: entry(loader, context, "glViewport")?,
                clear_colour: entry(loader, context, "glClearColor")?,
                clear: entry(loader, context, "glClear")?,
                draw_arrays: entry(loader, context, "glDrawArrays")?,
                active_texture: entry(loader, context, "glActiveTexture")?,
                get_integerv: entry(loader, context, "glGetIntegerv")?,
                get_stringi: entry(loader, context, "glGetStringi")?,
            }
        };

        let program = link_program(&entries)?;
        let (picture_uniform, flip_y_uniform) = unsafe {
            let picture = CString::new("picture").unwrap();
            let flip_y = CString::new("flip_y").unwrap();
            (
                (entries.get_uniform_location)(program, picture.as_ptr()),
                (entries.get_uniform_location)(program, flip_y.as_ptr()),
            )
        };

        let mut vertex_array = 0u32;
        let mut vertex_buffer = 0u32;
        let mut texture = 0u32;
        let client_storage;
        unsafe {
            (entries.gen_vertex_arrays)(1, &mut vertex_array);
            (entries.bind_vertex_array)(vertex_array);
            (entries.gen_buffers)(1, &mut vertex_buffer);
            (entries.bind_buffer)(GL_ARRAY_BUFFER, vertex_buffer);
            (entries.buffer_data)(
                GL_ARRAY_BUFFER,
                std::mem::size_of_val(&QUAD) as isize,
                QUAD.as_ptr() as *const c_void,
                GL_STATIC_DRAW,
            );
            let stride = FLOATS_PER_VERTEX * std::mem::size_of::<f32>() as i32;
            (entries.vertex_attrib_pointer)(
                POSITION_ATTRIBUTE,
                COMPONENTS_PER_ATTRIBUTE,
                GL_FLOAT,
                GL_FALSE,
                stride,
                std::ptr::null(),
            );
            (entries.enable_vertex_attrib_array)(POSITION_ATTRIBUTE);
            (entries.vertex_attrib_pointer)(
                TEXTURE_COORDINATE_ATTRIBUTE,
                COMPONENTS_PER_ATTRIBUTE,
                GL_FLOAT,
                GL_FALSE,
                stride,
                (COMPONENTS_PER_ATTRIBUTE as usize * std::mem::size_of::<f32>()) as *const c_void,
            );
            (entries.enable_vertex_attrib_array)(TEXTURE_COORDINATE_ATTRIBUTE);
            (entries.bind_vertex_array)(0);

            (entries.gen_textures)(1, &mut texture);
            (entries.bind_texture)(GL_TEXTURE_2D, texture);
            (entries.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (entries.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (entries.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (entries.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            client_storage = has_extension(&entries, b"GL_APPLE_client_storage");
            if client_storage {
                (entries.tex_parameteri)(
                    GL_TEXTURE_2D,
                    GL_TEXTURE_STORAGE_HINT_APPLE,
                    GL_STORAGE_SHARED_APPLE,
                );
                (entries.pixel_storei)(GL_UNPACK_CLIENT_STORAGE_APPLE, GL_TRUE);
            }
            (entries.bind_texture)(GL_TEXTURE_2D, 0);
        }
        if client_storage {
            tracing::info!(
                "grok preview: GL_APPLE_client_storage, decoded frames stay in client memory"
            );
        }

        Ok(GlPresenter {
            entries,
            program,
            vertex_array,
            texture,
            texture_size: (0, 0),
            uploaded_serial: 0,
            picture_uniform,
            flip_y_uniform,
            client_storage,
            backing: None,
        })
    }

    // the duration is the texture upload, None when this serial was already uploaded
    pub fn draw(
        &mut self,
        framebuffer: i32,
        width: i32,
        height: i32,
        flip_y: bool,
        frame: Option<Arc<ComposedFrame>>,
        serial: u64,
    ) -> Result<Option<Duration>, String> {
        unsafe {
            let entries = &self.entries;
            (entries.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer as u32);
            (entries.viewport)(0, 0, width, height);
            (entries.clear_colour)(0.0, 0.0, 0.0, 1.0);
            (entries.clear)(GL_COLOR_BUFFER_BIT);
        }
        let Some(frame) = frame else {
            return Ok(None);
        };
        if width <= 0 || height <= 0 {
            return Ok(None);
        }
        let Some(rectangle) =
            picture_rectangle(width as u32, height as u32, frame.width, frame.height)
        else {
            return Ok(None);
        };

        let uploaded = self.upload(frame, serial);
        let entries = &self.entries;
        unsafe {
            // gl counts rows from the bottom of the framebuffer
            (entries.viewport)(
                rectangle.x as i32,
                height - rectangle.y as i32 - rectangle.height as i32,
                rectangle.width as i32,
                rectangle.height as i32,
            );
            (entries.use_program)(self.program);
            (entries.active_texture)(GL_TEXTURE0);
            (entries.bind_texture)(GL_TEXTURE_2D, self.texture);
            (entries.uniform1i)(self.picture_uniform, PICTURE_TEXTURE_UNIT);
            (entries.uniform1i)(self.flip_y_uniform, i32::from(flip_y));
            (entries.bind_vertex_array)(self.vertex_array);
            (entries.draw_arrays)(GL_TRIANGLE_STRIP, 0, QUAD_VERTICES);
            (entries.bind_vertex_array)(0);
        }
        Ok(uploaded)
    }

    fn upload(&mut self, frame: Arc<ComposedFrame>, serial: u64) -> Option<Duration> {
        let size = (frame.width, frame.height);
        if serial == self.uploaded_serial && size == self.texture_size {
            return None;
        }
        let started = Instant::now();
        let pixels = frame.data().as_ptr() as *const c_void;
        let entries = &self.entries;
        unsafe {
            (entries.bind_texture)(GL_TEXTURE_2D, self.texture);
            (entries.pixel_storei)(GL_UNPACK_ALIGNMENT, RGBA_BYTES_PER_PIXEL as i32);
            // client storage: TexImage2D keeps this pointer as the store, no copy.
            // otherwise SubImage copies into the existing allocation when the size matches.
            if self.client_storage || size != self.texture_size {
                (entries.tex_image_2d)(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA8,
                    frame.width as i32,
                    frame.height as i32,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    pixels,
                );
            } else {
                (entries.tex_sub_image_2d)(
                    GL_TEXTURE_2D,
                    0,
                    0,
                    0,
                    frame.width as i32,
                    frame.height as i32,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    pixels,
                );
            }
        }
        self.texture_size = size;
        self.uploaded_serial = serial;
        self.backing = Some(frame);
        Some(started.elapsed())
    }
}

fn has_extension(entries: &Entries, name: &[u8]) -> bool {
    let mut count = 0i32;
    unsafe { (entries.get_integerv)(GL_NUM_EXTENSIONS, &mut count) };
    for index in 0..count.max(0) as u32 {
        let pointer = unsafe { (entries.get_stringi)(GL_EXTENSIONS, index) };
        if pointer.is_null() {
            continue;
        }
        let extension = unsafe { CStr::from_ptr(pointer as *const c_char) };
        if extension.to_bytes() == name {
            return true;
        }
    }
    false
}

fn link_program(entries: &Entries) -> Result<u32, String> {
    let vertex = compile(entries, GL_VERTEX_SHADER, VERTEX_SHADER)?;
    let fragment = compile(entries, GL_FRAGMENT_SHADER, FRAGMENT_SHADER)?;
    unsafe {
        let program = (entries.create_program)();
        (entries.attach_shader)(program, vertex);
        (entries.attach_shader)(program, fragment);
        (entries.link_program)(program);
        (entries.delete_shader)(vertex);
        (entries.delete_shader)(fragment);
        let mut linked = 0i32;
        (entries.get_programiv)(program, GL_LINK_STATUS, &mut linked);
        if linked == 0 {
            let mut log = vec![0u8; INFO_LOG_BYTES as usize];
            let mut written = 0i32;
            (entries.get_program_info_log)(
                program,
                INFO_LOG_BYTES,
                &mut written,
                log.as_mut_ptr() as *mut c_char,
            );
            log.truncate(written.max(0) as usize);
            return Err(format!(
                "the picture shader did not link: {}",
                String::from_utf8_lossy(&log)
            ));
        }
        Ok(program)
    }
}

fn compile(entries: &Entries, kind: u32, source: &str) -> Result<u32, String> {
    let text = CString::new(source).map_err(|_| "shader source holds a NUL byte".to_string())?;
    unsafe {
        let shader = (entries.create_shader)(kind);
        if shader == 0 {
            return Err("glCreateShader returned no shader".to_string());
        }
        let pointer = text.as_ptr();
        (entries.shader_source)(shader, 1, &pointer, std::ptr::null());
        (entries.compile_shader)(shader);
        let mut compiled = 0i32;
        (entries.get_shaderiv)(shader, GL_COMPILE_STATUS, &mut compiled);
        if compiled == 0 {
            let mut log = vec![0u8; INFO_LOG_BYTES as usize];
            let mut written = 0i32;
            (entries.get_shader_info_log)(
                shader,
                INFO_LOG_BYTES,
                &mut written,
                log.as_mut_ptr() as *mut c_char,
            );
            log.truncate(written.max(0) as usize);
            (entries.delete_shader)(shader);
            return Err(format!(
                "the picture shader did not compile: {}",
                String::from_utf8_lossy(&log)
            ));
        }
        Ok(shader)
    }
}
