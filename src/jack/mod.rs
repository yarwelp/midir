use std::{mem, slice};

use jack_sys::jack_nframes_t;

mod wrappers;
use self::wrappers::*;

use ::{Ignore, MidiMessage};
use ::{InitError, PortInfoError, ConnectError, ConnectErrorKind, SendError};
use ::traits::*;

const OUTPUT_RINGBUFFER_SIZE: usize = 16384;

struct InputHandlerData<T> {
    port: Option<MidiPort>,
    message: MidiMessage, // TODO: do we need to cache message contents across callback calls? if not, remove this
    last_time: Option<u64>,
    ignore_flags: Ignore,
    callback: Box<FnMut(f64, &[u8], &mut T)+Send>,
    user_data: Option<T>
}

pub struct MidiInput {
    ignore_flags: Ignore,
    client: Option<Client>,
}

pub struct MidiInputConnection<T> {
    handler_data: Box<InputHandlerData<T>>,
    client: Option<Client>
}

impl MidiInput {
    pub fn new(client_name: &str) -> Result<Self, InitError> {
        let client = match Client::open(client_name, NoStartServer) {
            Ok(c) => c,
            Err(_) => { return Err(InitError); } // TODO: maybe add message that Jack server might not be running
        };
        
        Ok(MidiInput {
            ignore_flags: Ignore::None,
            client: Some(client),
        })
    }
    
    pub fn ignore(&mut self, flags: Ignore) {
        self.ignore_flags = flags;
    }
    
	pub fn port_count(&self) -> u32 {
        self.client.as_ref().unwrap().get_midi_ports(PortIsOutput).count() as u32
    }
    
    pub fn port_name(&self, port_number: u32) -> Result<String, PortInfoError> {
        let midi_ports = self.client.as_ref().unwrap().get_midi_ports(PortIsOutput);
        let port_number = port_number as usize;
        if port_number >= midi_ports.count() {
            Err(PortInfoError::PortNumberOutOfRange)
        } else {
            Ok(midi_ports[port_number].to_string())
        }
    }
    
    fn activate_callback<F, T: Send>(&mut self, callback: F, data: T)
            -> Box<InputHandlerData<T>>
            where F: FnMut(f64, &[u8], &mut T) + Send + 'static
    {
        let handler_data = Box::new(InputHandlerData {
            port: None,
            message: MidiMessage::new(),
            last_time: None,
            ignore_flags: self.ignore_flags,
            callback: Box::new(callback),
            user_data: Some(data)
        });
        
        let data_ptr = unsafe { mem::transmute_copy::<_, *mut InputHandlerData<T>>(&handler_data) };
        
        self.client.as_mut().unwrap().set_process_callback(handle_input::<T>, data_ptr as *mut ::libc::c_void);
        self.client.as_mut().unwrap().activate();
        handler_data
    }
    
    pub fn connect<F, T: Send>(
        mut self, port_number: u32, port_name: &str, callback: F, data: T
    ) -> Result<MidiInputConnection<T>, ConnectError<MidiInput>>
        where F: FnMut(f64, &[u8], &mut T) + Send + 'static {
        
        let source_port_name = {
            let ports = self.client.as_ref().unwrap().get_midi_ports(PortIsOutput);
            let port_number = port_number as usize;
            if port_number >= ports.count() {
                None
            } else {
                Some(ports.get_c_name(port_number).to_owned()) // have to copy the name to prevent borrowing issues
            }
        };
        
        let source_port_name = match source_port_name {
            None => return Err(ConnectError::new(ConnectErrorKind::PortNumberOutOfRange, self)),
            Some(s) => s
        };
        
        let mut handler_data = self.activate_callback(callback, data);
        
        // Create port ...
        let port = match self.client.as_mut().unwrap().register_midi_port(port_name, PortIsInput) {
            Ok(p) => p,
            Err(()) => { return Err(ConnectError::other("could not register JACK port", self)); }
        };
        
        // ... and connect it to the output
        self.client.as_mut().unwrap().connect(&source_port_name, port.get_name());
        
        handler_data.port = Some(port);
        
        Ok(MidiInputConnection {
            handler_data: handler_data,
            client: self.client.take()
        })
    }
}

impl PortInfo for MidiInput {
    fn new(client_name: &str) -> Result<Self, InitError> {
        Self::new(client_name)
    }
    
    fn port_count(&self) -> u32 {
        self.port_count()
    }
    
    fn port_name(&self, port_number: u32) -> Result<String, PortInfoError> {
        self.port_name(port_number)
    }
}

impl<T: Send> InputConnect<T> for MidiInput {
    type Connection = MidiInputConnection<T>; 
    
    fn connect<F>(
        self, port_number: u32, port_name: &str, callback: F, data: T
    ) -> Result<Self::Connection, ConnectError<Self>>
    where F: FnMut(f64, &[u8], &mut T) + Send + 'static {
        self.connect(port_number, port_name, callback, data)
    }
}

impl<T: Send> ::os::nix::VirtualInput<T> for MidiInput {
    type Connection = MidiInputConnection<T>;
    
    fn create_virtual<F>(
        mut self, port_name: &str, callback: F, data: T
    ) -> Result<Self::Connection, ConnectError<Self>>
    where F: FnMut(f64, &[u8], &mut T) + Send + 'static {
    
        let mut handler_data = self.activate_callback(callback, data);
        
        // Create port
        let port = match self.client.as_mut().unwrap().register_midi_port(port_name, PortIsInput) {
            Ok(p) => p,
            Err(()) => { return Err(ConnectError::other("could not register JACK port", self)); }
        };
        
        handler_data.port = Some(port);
        
        Ok(MidiInputConnection {
            handler_data: handler_data,
            client: self.client.take()
        })
    }
}

impl<T> MidiInputConnection<T> {
    pub fn close(mut self) -> (MidiInput, T) {
        self.close_internal();
        
        (MidiInput {
            client: self.client.take(),
            ignore_flags: self.handler_data.ignore_flags,
        }, self.handler_data.user_data.take().unwrap())
    }
    
    fn close_internal(&mut self) {
        let port = self.handler_data.port.take().unwrap();
        self.client.as_mut().unwrap().unregister_midi_port(port);
        self.client.as_mut().unwrap().deactivate();
    }
}

impl<T> Drop for MidiInputConnection<T> {
    fn drop(&mut self) {
        if self.client.is_some() {
            self.close_internal();
        }
    }
}

impl<T> InputConnection<T> for MidiInputConnection<T> {
    type Input = MidiInput;
    
    fn close(self) -> (Self::Input, T) {
        self.close()
    }
}

extern "C" fn handle_input<T>(nframes: jack_nframes_t, arg: *mut ::libc::c_void) -> i32 {
    let data: &mut InputHandlerData<T> = unsafe { mem::transmute(arg) }; 
    
    // Is port created?
    if let Some(ref port) = data.port {
        let buff = port.get_midi_buffer(nframes);
        
        let mut message = MidiMessage::new();
        
        // We have midi events in buffer
        let evcount = buff.get_event_count();
        let mut event = unsafe { mem::uninitialized() };
        
        for j in 0..evcount {
            message.bytes.clear();
            
            unsafe { buff.get_event(&mut event, j) };
            
            for i in 0..event.size {
                message.bytes.push(unsafe { *event.buffer.offset(i as isize) });
            }
            
            // Compute the delta time.
            let time = Client::get_time();
            message.timestamp = match data.last_time {
                None => 0.0,
                Some(last) => (time - last) as f64 * 0.000001
            };
            data.last_time = Some(time);
            
            (data.callback)(message.timestamp, &message.bytes, data.user_data.as_mut().unwrap());
        }
    }
    
    return 0;
}

struct OutputHandlerData {
    port: Option<MidiPort>,
    buff_size: Ringbuffer,
    buff_message: Ringbuffer,
}

pub struct MidiOutput {
    client: Option<Client>,
}

pub struct MidiOutputConnection {
    handler_data: Box<OutputHandlerData>,
    client: Option<Client>
}

impl MidiOutput {
    pub fn new(client_name: &str) -> Result<Self, InitError> {
        let client = match Client::open(client_name, NoStartServer) {
            Ok(c) => c,
            Err(_) => { return Err(InitError); } // TODO: maybe add message that Jack server might not be running
        };
        
        Ok(MidiOutput {
            client: Some(client),
        })
    }
    
	pub fn port_count(&self) -> u32 {
        self.client.as_ref().unwrap().get_midi_ports(PortIsInput).count() as u32
    }
    
    pub fn port_name(&self, port_number: u32) -> Result<String, PortInfoError> {
        let midi_ports = self.client.as_ref().unwrap().get_midi_ports(PortIsInput);
        let port_number = port_number as usize;
        if port_number >= midi_ports.count() {
            Err(PortInfoError::PortNumberOutOfRange)
        } else {
            Ok(midi_ports[port_number].to_string())
        }
    }
    
    fn activate_callback(&mut self) -> Box<OutputHandlerData> {
        let handler_data = Box::new(OutputHandlerData {
            port: None,
            buff_size: Ringbuffer::new(OUTPUT_RINGBUFFER_SIZE),
            buff_message: Ringbuffer::new(OUTPUT_RINGBUFFER_SIZE)
        });
        
        let data_ptr = unsafe { mem::transmute_copy::<_, *mut OutputHandlerData>(&handler_data) };
        
        self.client.as_mut().unwrap().set_process_callback(handle_output, data_ptr as *mut ::libc::c_void);
        self.client.as_mut().unwrap().activate();
        handler_data
    }
    
    pub fn connect(mut self, port_number: u32, port_name: &str) -> Result<MidiOutputConnection, ConnectError<MidiOutput>> {
        let dest_port_name = {
            let ports = self.client.as_ref().unwrap().get_midi_ports(PortIsInput);
            let port_number = port_number as usize;
            if port_number >= ports.count() {
                None
            } else {
                Some(ports.get_c_name(port_number).to_owned()) // have to copy the name to prevent borrowing issues
            }
        };
        
        let dest_port_name = match dest_port_name {
            None => return Err(ConnectError::new(ConnectErrorKind::PortNumberOutOfRange, self)),
            Some(s) => s
        };
        
        let mut handler_data = self.activate_callback();
        
        // Create port ...
        let port = match self.client.as_mut().unwrap().register_midi_port(port_name, PortIsOutput) {
            Ok(p) => p,
            Err(()) => { return Err(ConnectError::other("could not register JACK port", self)); }
        };
        
        // ... and connect it to the input
        self.client.as_mut().unwrap().connect(port.get_name(), &dest_port_name);
        
        handler_data.port = Some(port);
        
        Ok(MidiOutputConnection {
            handler_data: handler_data,
            client: self.client.take()
        })
    }
}

impl PortInfo for MidiOutput {
    fn new(client_name: &str) -> Result<Self, super::InitError> {
        Self::new(client_name)
    }
    
    fn port_count(&self) -> u32 {
        self.port_count()
    }
    
    fn port_name(&self, port_number: u32) -> Result<String, PortInfoError> {
        self.port_name(port_number)
    }
}

impl OutputConnect for MidiOutput {
    type Connection = MidiOutputConnection; 
    
     fn connect(
        self, port_number: u32, port_name: &str
    ) -> Result<Self::Connection, super::ConnectError<Self>> {
        self.connect(port_number, port_name)
    }
}
impl ::os::nix::VirtualOutput for MidiOutput {
    type Connection = MidiOutputConnection;
    
    fn create_virtual(
        mut self, port_name: &str
    ) -> Result<Self::Connection, ConnectError<Self>> {
        let mut handler_data = self.activate_callback();
        
        // Create port
        let port = match self.client.as_mut().unwrap().register_midi_port(port_name, PortIsOutput) {
            Ok(p) => p,
            Err(()) => { return Err(ConnectError::other("could not register JACK port", self)); }
        };
        
        handler_data.port = Some(port);
        
        Ok(MidiOutputConnection {
            handler_data: handler_data,
            client: self.client.take()
        })
    }
}

impl MidiOutputConnection {
    pub fn send(&mut self, message: &[u8]) -> Result<(), SendError> {
        let nbytes = message.len();
        
        // Write full message to buffer
        let written = self.handler_data.buff_message.write(message);
        debug_assert!(written == nbytes, "not enough bytes written to ALSA ringbuffer `message`");
        let nbytes_slice = unsafe { slice::from_raw_parts(&nbytes as *const usize as *const u8, mem::size_of_val(&nbytes)) }; 
        let written = self.handler_data.buff_size.write(nbytes_slice);
        debug_assert!(written == mem::size_of_val(&nbytes), "not enough bytes written to ALSA ringbuffer `size`");
        Ok(())
    }
    
    pub fn close(mut self) -> MidiOutput {
        self.close_internal();
        
        MidiOutput {
            client: self.client.take(),
        }
    }
    
    fn close_internal(&mut self) {
        let port = self.handler_data.port.take().unwrap();
        self.client.as_mut().unwrap().unregister_midi_port(port);
        self.client.as_mut().unwrap().deactivate();
    }
}

impl Drop for MidiOutputConnection {
    fn drop(&mut self) {
        if self.client.is_some() {
            self.close_internal();
        }
    }
}

impl OutputConnection for MidiOutputConnection {
    type Output = MidiOutput;
    
    fn close(self) -> Self::Output {
        self.close()
    }
    
    fn send(&mut self, message: &[u8]) -> Result<(), SendError> {
        self.send(message)
    }   
}

extern "C" fn handle_output(nframes: jack_nframes_t, arg: *mut ::libc::c_void) -> i32 {
    let data: &mut OutputHandlerData = unsafe { mem::transmute(arg) }; 
    
    // Is port created?
    if let Some(ref port) = data.port {
        let mut space: usize = 0;
        
        let mut buff = port.get_midi_buffer(nframes);
        buff.clear();
        
        while data.buff_size.get_read_space() > 0 {
            let read = data.buff_size.read(&mut space as *mut usize as *mut u8, mem::size_of::<usize>());
            debug_assert!(read == mem::size_of::<usize>(), "not enough bytes read from `size` ringbuffer");
            let midi_data = buff.event_reserve(0, space);
            let read = data.buff_message.read(midi_data, space);
            debug_assert!(read == space, "not enough bytes read from `message` ringbuffer");
        }
    }
    
    return 0;
}