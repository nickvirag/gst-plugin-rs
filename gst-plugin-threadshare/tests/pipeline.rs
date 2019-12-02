// Copyright (C) 2019 François Laignel <fengalin@free.fr>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst;
use gst::prelude::*;

use std::sync::mpsc;

use gstthreadshare;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare pipeline test");
    });
}

#[test]
fn test_multiple_contexts() {
    use std::net;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    init();

    const CONTEXT_NB: u32 = 2;
    const SRC_NB: u16 = 4;
    const CONTEXT_WAIT: u32 = 1;
    const BUFFER_NB: u32 = 3;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let mut src_list = Vec::<gst::Element>::new();

    for i in 0..SRC_NB {
        let src =
            gst::ElementFactory::make("ts-udpsrc", Some(format!("src-{}", i).as_str())).unwrap();
        src.set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        src.set_property("context-wait", &CONTEXT_WAIT).unwrap();
        src.set_property("port", &(40000u32 + (i as u32))).unwrap();

        let queue =
            gst::ElementFactory::make("ts-queue", Some(format!("queue-{}", i).as_str())).unwrap();
        queue
            .set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        queue.set_property("context-wait", &CONTEXT_WAIT).unwrap();

        let appsink =
            gst::ElementFactory::make("appsink", Some(format!("sink-{}", i).as_str())).unwrap();

        pipeline.add_many(&[&src, &queue, &appsink]).unwrap();
        gst::Element::link_many(&[&src, &queue, &appsink]).unwrap();

        appsink.set_property("emit-signals", &true).unwrap();
        appsink
            .set_property("async", &glib::Value::from(&false))
            .unwrap();

        let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
        appsink.connect_new_sample(move |appsink| {
            let _sample = appsink
                .emit("pull-sample", &[])
                .unwrap()
                .unwrap()
                .get::<gst::Sample>()
                .unwrap()
                .unwrap();

            Ok(gst::FlowSuccess::Ok)
        });

        src_list.push(src);
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut test_scenario = Some(move || {
        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let destinations = (40000..(40000 + SRC_NB))
            .map(|port| SocketAddr::new(ipaddr, port))
            .collect::<Vec<_>>();

        let wait = std::time::Duration::from_millis(CONTEXT_WAIT as u64);

        for _ in 0..BUFFER_NB {
            let now = std::time::Instant::now();

            for dest in &destinations {
                socket.send_to(&buffer, dest).unwrap();
            }

            let elapsed = now.elapsed();
            if elapsed < wait {
                std::thread::sleep(wait - elapsed);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(50));

        pipeline_clone.set_state(gst::State::Null).unwrap();
        l_clone.quit();
    });

    let bus = pipeline.get_bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state_changed) => {
                if let Some(source) = state_changed.get_src() {
                    if source.get_type() == gst::Pipeline::static_type() {
                        if state_changed.get_old() == gst::State::Paused
                            && state_changed.get_current() == gst::State::Playing
                        {
                            if let Some(test_scenario) = test_scenario.take() {
                                std::thread::spawn(test_scenario);
                            }
                        }
                    }
                }
            }
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    err.get_src().map(|s| s.get_path_string()),
                    err.get_error(),
                    err.get_debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    println!("starting...");

    l.run();
}

#[test]
fn test_multiple_contexts_proxy() {
    use std::net;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    init();

    const CONTEXT_NB: u32 = 2;
    const SRC_NB: u16 = 4;
    const CONTEXT_WAIT: u32 = 1;
    const BUFFER_NB: u32 = 3;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let mut src_list = Vec::<gst::Element>::new();

    for i in 0..SRC_NB {
        let src =
            gst::ElementFactory::make("ts-udpsrc", Some(format!("src-{}", i).as_str())).unwrap();
        src.set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        src.set_property("context-wait", &CONTEXT_WAIT).unwrap();
        src.set_property("port", &(40000u32 + (i as u32))).unwrap();

        let proxysink = gst::ElementFactory::make("ts-proxysink", None).unwrap();
        proxysink
            .set_property("proxy-context", &format!("proxy-{}", i))
            .unwrap();
        let proxysrc = gst::ElementFactory::make("ts-proxysrc", None).unwrap();
        proxysrc
            .set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        proxysrc
            .set_property("proxy-context", &format!("proxy-{}", i))
            .unwrap();

        let appsink =
            gst::ElementFactory::make("appsink", Some(format!("sink-{}", i).as_str())).unwrap();

        pipeline
            .add_many(&[&src, &proxysink, &proxysrc, &appsink])
            .unwrap();
        src.link(&proxysink).unwrap();
        proxysrc.link(&appsink).unwrap();

        appsink.set_property("emit-signals", &true).unwrap();
        appsink
            .set_property("async", &glib::Value::from(&false))
            .unwrap();

        let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
        appsink.connect_new_sample(move |appsink| {
            let _sample = appsink
                .emit("pull-sample", &[])
                .unwrap()
                .unwrap()
                .get::<gst::Sample>()
                .unwrap()
                .unwrap();

            Ok(gst::FlowSuccess::Ok)
        });

        src_list.push(src);
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut test_scenario = Some(move || {
        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let destinations = (40000..(40000 + SRC_NB))
            .map(|port| SocketAddr::new(ipaddr, port))
            .collect::<Vec<_>>();

        let wait = std::time::Duration::from_millis(CONTEXT_WAIT as u64);

        for _ in 0..BUFFER_NB {
            let now = std::time::Instant::now();

            for dest in &destinations {
                socket.send_to(&buffer, dest).unwrap();
            }

            let elapsed = now.elapsed();
            if elapsed < wait {
                std::thread::sleep(wait - elapsed);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(50));

        pipeline_clone.set_state(gst::State::Null).unwrap();
        l_clone.quit();
    });

    let bus = pipeline.get_bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state_changed) => {
                if let Some(source) = state_changed.get_src() {
                    if source.get_type() == gst::Pipeline::static_type() {
                        if state_changed.get_old() == gst::State::Paused
                            && state_changed.get_current() == gst::State::Playing
                        {
                            if let Some(test_scenario) = test_scenario.take() {
                                std::thread::spawn(test_scenario);
                            }
                        }
                    }
                }
            }
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    err.get_src().map(|s| s.get_path_string()),
                    err.get_error(),
                    err.get_debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    println!("starting...");
    l.run();
}

#[test]
fn test_eos() {
    const CONTEXT: &str = "test_eos";

    init();

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let caps = gst::Caps::new_simple("foo/bar", &[]);

    let src = gst::ElementFactory::make("ts-appsrc", Some("ts-appsrc")).unwrap();
    src.set_property("caps", &caps).unwrap();
    src.set_property("do-timestamp", &true).unwrap();
    src.set_property("context", &CONTEXT).unwrap();

    let queue = gst::ElementFactory::make("ts-queue", None).unwrap();
    queue.set_property("context", &CONTEXT).unwrap();

    let appsink = gst::ElementFactory::make("appsink", None).unwrap();

    pipeline.add_many(&[&src, &queue, &appsink]).unwrap();
    gst::Element::link_many(&[&src, &queue, &appsink]).unwrap();

    appsink.set_property("emit-signals", &true).unwrap();
    appsink
        .set_property("async", &glib::Value::from(&false))
        .unwrap();

    let (sample_notifier, sample_notif_rcv) = mpsc::channel();
    let (eos_notifier, eos_notif_rcv) = mpsc::channel();

    let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.connect_new_sample(move |appsink| {
        let _ = appsink
            .emit("pull-sample", &[])
            .unwrap()
            .unwrap()
            .get::<gst::Sample>()
            .unwrap()
            .unwrap();

        sample_notifier.send(()).unwrap();

        Ok(gst::FlowSuccess::Ok)
    });

    appsink.connect_eos(move |_appsink| eos_notifier.send(()).unwrap());

    fn push_buffer(src: &gst::Element) -> bool {
        src.emit("push-buffer", &[&gst::Buffer::from_slice(vec![0; 1024])])
            .unwrap()
            .unwrap()
            .get_some::<bool>()
            .unwrap()
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut scenario = Some(move || {
        // Initialize the dataflow
        assert!(push_buffer(&src));

        sample_notif_rcv.recv().unwrap();

        assert!(src
            .emit("end-of-stream", &[])
            .unwrap()
            .unwrap()
            .get_some::<bool>()
            .unwrap());

        eos_notif_rcv.recv().unwrap();

        assert!(push_buffer(&src));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            sample_notif_rcv.try_recv().unwrap_err(),
            mpsc::TryRecvError::Empty
        );

        pipeline_clone.set_state(gst::State::Null).unwrap();
        l_clone.quit();
    });

    let l_clone = l.clone();
    pipeline.get_bus().unwrap().add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state_changed) => {
                if let Some(source) = state_changed.get_src() {
                    if source.get_type() != gst::Pipeline::static_type() {
                        return glib::Continue(true);
                    }
                    if state_changed.get_old() == gst::State::Paused
                        && state_changed.get_current() == gst::State::Playing
                    {
                        if let Some(scenario) = scenario.take() {
                            std::thread::spawn(scenario);
                        }
                    }
                }
            }
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    err.get_src().map(|s| s.get_path_string()),
                    err.get_error(),
                    err.get_debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    println!("Starting main loop...");
    l.run();
    println!("Stopped main loop");
}

#[test]
fn test_premature_shutdown() {
    init();

    const APPSRC_CONTEXT_WAIT: u32 = 0;
    const QUEUE_CONTEXT_WAIT: u32 = 1;
    const QUEUE_ITEMS_CAPACITY: u32 = 1;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let caps = gst::Caps::new_simple("foo/bar", &[]);

    let src = gst::ElementFactory::make("ts-appsrc", Some("ts-appsrc")).unwrap();
    src.set_property("caps", &caps).unwrap();
    src.set_property("do-timestamp", &true).unwrap();
    src.set_property("context", &"appsrc-context").unwrap();
    src.set_property("context-wait", &APPSRC_CONTEXT_WAIT)
        .unwrap();

    let queue = gst::ElementFactory::make("ts-queue", None).unwrap();
    queue.set_property("context", &"queue-context").unwrap();
    queue
        .set_property("context-wait", &QUEUE_CONTEXT_WAIT)
        .unwrap();
    queue
        .set_property("max-size-buffers", &QUEUE_ITEMS_CAPACITY)
        .unwrap();

    let appsink = gst::ElementFactory::make("appsink", None).unwrap();

    pipeline.add_many(&[&src, &queue, &appsink]).unwrap();
    gst::Element::link_many(&[&src, &queue, &appsink]).unwrap();

    appsink.set_property("emit-signals", &true).unwrap();
    appsink
        .set_property("async", &glib::Value::from(&false))
        .unwrap();

    let (sender, receiver) = mpsc::channel();

    let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.connect_new_sample(move |appsink| {
        let _sample = appsink
            .emit("pull-sample", &[])
            .unwrap()
            .unwrap()
            .get::<gst::Sample>()
            .unwrap()
            .unwrap();

        sender.send(()).unwrap();

        Ok(gst::FlowSuccess::Ok)
    });

    fn push_buffer(src: &gst::Element) -> bool {
        src.emit("push-buffer", &[&gst::Buffer::from_slice(vec![0; 1024])])
            .unwrap()
            .unwrap()
            .get_some::<bool>()
            .unwrap()
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut scenario = Some(move || {
        println!("\n**** STEP 1: Playing");
        // Initialize the dataflow
        assert!(push_buffer(&src));

        // Wait for the buffer to reach AppSink
        receiver.recv().unwrap();
        assert_eq!(receiver.try_recv().unwrap_err(), mpsc::TryRecvError::Empty);

        assert!(push_buffer(&src));

        pipeline_clone.set_state(gst::State::Paused).unwrap();

        // Paused -> can't push_buffer
        assert!(!push_buffer(&src));

        println!("\n**** STEP 2: Paused -> Playing");
        pipeline_clone.set_state(gst::State::Playing).unwrap();

        println!("\n**** STEP 3: Playing");

        receiver.recv().unwrap();

        push_buffer(&src);
        receiver.recv().unwrap();

        // Fill up the (dataqueue) and abruptly shutdown
        push_buffer(&src);
        push_buffer(&src);

        pipeline_clone.set_state(gst::State::Null).unwrap();

        assert_eq!(receiver.try_recv().unwrap_err(), mpsc::TryRecvError::Empty);
        l_clone.quit();
    });

    let l_clone = l.clone();
    pipeline.get_bus().unwrap().add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state_changed) => {
                if let Some(source) = state_changed.get_src() {
                    if source.get_type() != gst::Pipeline::static_type() {
                        return glib::Continue(true);
                    }
                    if state_changed.get_old() == gst::State::Paused
                        && state_changed.get_current() == gst::State::Playing
                    {
                        if let Some(scenario) = scenario.take() {
                            std::thread::spawn(scenario);
                        }
                    }
                }
            }
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    err.get_src().map(|s| s.get_path_string()),
                    err.get_error(),
                    err.get_debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    println!("Starting main loop...");
    l.run();
    println!("Stopped main loop");
}
