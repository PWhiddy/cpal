use super::com;
use super::ole32;
use super::winapi;
use super::Endpoint;
use super::check_result;

use std::cmp;
use std::slice;
use std::mem;
use std::ptr;
use std::marker::PhantomData;

use CreationError;
use Format;

pub struct Voice {
    audio_client: *mut winapi::IAudioClient,
    render_client: *mut winapi::IAudioRenderClient,
    max_frames_in_buffer: winapi::UINT32,
    num_channels: winapi::WORD,
    bytes_per_frame: winapi::WORD,
    samples_per_second: winapi::DWORD,
    bits_per_sample: winapi::WORD,
    playing: bool,
}

unsafe impl Send for Voice {}
unsafe impl Sync for Voice {}

impl Voice {
    pub fn new(end_point: &Endpoint, format: &Format) -> Result<Voice, CreationError> {
        // FIXME: release everything
        unsafe {
            // making sure that COM is initialized
            // it's not actually sure that this is required, but when in doubt do it
            com::com_initialized();

            // obtaining a `IAudioClient`
            let audio_client = match end_point.build_audioclient() {
                Err(ref e) if e.raw_os_error() == Some(winapi::AUDCLNT_E_DEVICE_INVALIDATED) =>
                    return Err(CreationError::DeviceNotAvailable),
                e => e.unwrap(),
            };

            // computing the format and initializing the device
            let format = {
                let format_attempt = winapi::WAVEFORMATEX {
                    wFormatTag: winapi::WAVE_FORMAT_PCM,
                    nChannels: format.channels as winapi::WORD,
                    nSamplesPerSec: format.samples_rate.0 as winapi::DWORD,
                    nAvgBytesPerSec: format.channels as winapi::DWORD *
                                     format.samples_rate.0 as winapi::DWORD *
                                     format.data_type.get_sample_size() as winapi::DWORD,
                    nBlockAlign: format.channels as winapi::WORD *
                                 format.data_type.get_sample_size() as winapi::WORD,
                    wBitsPerSample: 8 * format.data_type.get_sample_size() as winapi::WORD,
                    cbSize: 0,
                };

                let mut format_ptr: *mut winapi::WAVEFORMATEX = mem::uninitialized();
                let hresult = (*audio_client).IsFormatSupported(winapi::AUDCLNT_SHAREMODE::AUDCLNT_SHAREMODE_SHARED,
                                                                &format_attempt, &mut format_ptr);

                if hresult == winapi::S_FALSE {
                    return Err(CreationError::FormatNotSupported);
                }

                match check_result(hresult) {
                    Err(ref e) if e.raw_os_error() == Some(winapi::AUDCLNT_E_DEVICE_INVALIDATED) =>
                    {
                        (*audio_client).Release();
                        return Err(CreationError::DeviceNotAvailable);
                    },
                    Err(e) => {
                        (*audio_client).Release();
                        panic!("{:?}", e);
                    },
                    Ok(()) => (),
                };


                let format = if format_ptr.is_null() {
                    &format_attempt
                } else {
                    &*format_ptr
                };

                let format_copy = ptr::read(format);

                let hresult = (*audio_client).Initialize(winapi::AUDCLNT_SHAREMODE::AUDCLNT_SHAREMODE_SHARED,
                                                         0, 10000000, 0, format, ptr::null());

                if !format_ptr.is_null() {
                    ole32::CoTaskMemFree(format_ptr as *mut _);
                }

                match check_result(hresult) {
                    Err(ref e) if e.raw_os_error() == Some(winapi::AUDCLNT_E_DEVICE_INVALIDATED) =>
                    {
                        (*audio_client).Release();
                        return Err(CreationError::DeviceNotAvailable);
                    },
                    Err(e) => {
                        (*audio_client).Release();
                        panic!("{:?}", e);
                    },
                    Ok(()) => (),
                };

                format_copy
            };

            // 
            let max_frames_in_buffer = {
                let mut max_frames_in_buffer = mem::uninitialized();
                let hresult = (*audio_client).GetBufferSize(&mut max_frames_in_buffer);

                match check_result(hresult) {
                    Err(ref e) if e.raw_os_error() == Some(winapi::AUDCLNT_E_DEVICE_INVALIDATED) =>
                    {
                        (*audio_client).Release();
                        return Err(CreationError::DeviceNotAvailable);
                    },
                    Err(e) => {
                        (*audio_client).Release();
                        panic!("{:?}", e);
                    },
                    Ok(()) => (),
                };

                max_frames_in_buffer
            };

            // 
            let render_client = {
                let mut render_client: *mut winapi::IAudioRenderClient = mem::uninitialized();
                let hresult = (*audio_client).GetService(&winapi::IID_IAudioRenderClient,
                                                         &mut render_client as *mut *mut winapi::IAudioRenderClient
                                                                            as *mut _);

                match check_result(hresult) {
                    Err(ref e) if e.raw_os_error() == Some(winapi::AUDCLNT_E_DEVICE_INVALIDATED) =>
                    {
                        (*audio_client).Release();
                        return Err(CreationError::DeviceNotAvailable);
                    },
                    Err(e) => {
                        (*audio_client).Release();
                        panic!("{:?}", e);
                    },
                    Ok(()) => (),
                };

                &mut *render_client
            };

            Ok(Voice {
                audio_client: audio_client,
                render_client: render_client,
                max_frames_in_buffer: max_frames_in_buffer,
                num_channels: format.nChannels,
                bytes_per_frame: format.nBlockAlign,
                samples_per_second: format.nSamplesPerSec,
                bits_per_sample: format.wBitsPerSample,
                playing: false,
            })
        }
    }

    pub fn get_channels(&self) -> ::ChannelsCount {
        self.num_channels as ::ChannelsCount
    }

    pub fn get_samples_rate(&self) -> ::SamplesRate {
        ::SamplesRate(self.samples_per_second as u32)
    }

    pub fn get_samples_format(&self) -> ::SampleFormat {
        match self.bits_per_sample {
            16 => ::SampleFormat::I16,
            _ => panic!("{}-bit format not yet supported", self.bits_per_sample),
        }
    }

    pub fn append_data<'a, T>(&'a mut self, max_elements: usize) -> Buffer<'a, T> {
        unsafe {
            loop {
                // 
                let frames_available = {
                    let mut padding = mem::uninitialized();
                    let hresult = (*self.audio_client).GetCurrentPadding(&mut padding);
                    check_result(hresult).unwrap();
                    self.max_frames_in_buffer - padding
                };

                if frames_available == 0 {
                    // TODO: 
                    ::std::thread::sleep_ms(1);
                    continue;
                }

                let frames_available = cmp::min(frames_available,
                                                max_elements as u32 * mem::size_of::<T>() as u32 /
                                                self.bytes_per_frame as u32);
                assert!(frames_available != 0);

                // loading buffer
                let (buffer_data, buffer_len) = {
                    let mut buffer: *mut winapi::BYTE = mem::uninitialized();
                    let hresult = (*self.render_client).GetBuffer(frames_available,
                                    &mut buffer as *mut *mut _);
                    check_result(hresult).unwrap();
                    assert!(!buffer.is_null());

                    (buffer as *mut T,
                     frames_available as usize * self.bytes_per_frame as usize
                          / mem::size_of::<T>())
                };

                let buffer = Buffer {
                    render_client: self.render_client,
                    buffer_data: buffer_data,
                    buffer_len: buffer_len,
                    frames: frames_available,
                    marker: PhantomData,
                };

                return buffer;
            }
        }
    }

    pub fn play(&mut self) {
        if !self.playing {
            unsafe {
                let hresult = (*self.audio_client).Start();
                check_result(hresult).unwrap();
            }
        }

        self.playing = true;
    }

    pub fn pause(&mut self) {
        if self.playing {
            unsafe {
                let hresult = (*self.audio_client).Stop();
                check_result(hresult).unwrap();
            }
        }

        self.playing = false;
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        unsafe {
            (*self.render_client).Release();
            (*self.audio_client).Release();
        }
    }
}

pub struct Buffer<'a, T: 'a> {
    render_client: *mut winapi::IAudioRenderClient,
    buffer_data: *mut T,
    buffer_len: usize,
    frames: winapi::UINT32,
    marker: PhantomData<&'a mut T>,
}

impl<'a, T> Buffer<'a, T> {
    pub fn get_buffer<'b>(&'b mut self) -> &'b mut [T] {
        unsafe {
            slice::from_raw_parts_mut(self.buffer_data, self.buffer_len)
        }
    }

    pub fn finish(self) {
        // releasing buffer
        unsafe {
            let hresult = (*self.render_client).ReleaseBuffer(self.frames as u32, 0);
            check_result(hresult).unwrap();
        };
    }
}
