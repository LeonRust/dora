#![allow(clippy::borrow_deref_ref)] // clippy warns about code generated by #[pymethods]

use std::sync::Arc;
use std::time::Duration;

use arrow::pyarrow::{FromPyArrow, ToPyArrow};
use dora_node_api::dora_core::config::NodeId;
use dora_node_api::merged::{MergeExternalSend, MergedEvent};
use dora_node_api::{DataflowId, DoraNode, EventStream};
use dora_operator_api_python::{pydict_to_metadata, DelayedCleanup, NodeCleanupHandle, PyEvent};
use dora_ros2_bridge_python::Ros2Subscription;
use eyre::Context;
use futures::{Stream, StreamExt};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_special_method_derive::{Dict, Dir, Repr, Str};

/// The custom node API lets you integrate `dora` into your application.
/// It allows you to retrieve input and send output in any fashion you want.
///
/// Use with:
///
/// ```python
/// from dora import Node
///
/// node = Node()
/// ```
///
/// :type node_id: str, optional
#[pyclass]
#[derive(Dir, Dict, Str, Repr)]
pub struct Node {
    events: Events,
    node: DelayedCleanup<DoraNode>,

    dataflow_id: DataflowId,
    node_id: NodeId,
}

#[pymethods]
impl Node {
    #[new]
    pub fn new(node_id: Option<String>) -> eyre::Result<Self> {
        let (node, events) = if let Some(node_id) = node_id {
            DoraNode::init_flexible(NodeId::from(node_id))
                .context("Could not setup node from node id. Make sure to have a running dataflow with this dynamic node")?
        } else {
            DoraNode::init_from_env().context("Couldn not initiate node from environment variable. For dynamic node, please add a node id in the initialization function.")?
        };

        let dataflow_id = *node.dataflow_id();
        let node_id = node.id().clone();
        let node = DelayedCleanup::new(node);
        let events = DelayedCleanup::new(events);
        let cleanup_handle = NodeCleanupHandle {
            _handles: Arc::new((node.handle(), events.handle())),
        };
        Ok(Node {
            events: Events {
                inner: EventsInner::Dora(events),
                cleanup_handle,
            },
            dataflow_id,
            node_id,
            node,
        })
    }

    /// `.next()` gives you the next input that the node has received.
    /// It blocks until the next event becomes available.
    /// You can use timeout in seconds to return if no input is available.
    /// It will return `None` when all senders has been dropped.
    ///
    /// ```python
    /// event = node.next()
    /// ```
    ///
    /// You can also iterate over the event stream with a loop
    ///
    /// ```python
    /// for event in node:
    ///    match event["type"]:
    ///        case "INPUT":
    ///            match event["id"]:
    ///                 case "image":
    /// ```
    ///
    /// :type timeout: float, optional
    /// :rtype: dict
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self, py: Python, timeout: Option<f32>) -> PyResult<Option<Py<PyDict>>> {
        let event = py.allow_threads(|| self.events.recv(timeout.map(Duration::from_secs_f32)));
        if let Some(event) = event {
            let dict = event
                .to_py_dict(py)
                .context("Could not convert event into a dict")?;
            Ok(Some(dict))
        } else {
            Ok(None)
        }
    }

    /// You can iterate over the event stream with a loop
    ///
    /// ```python
    /// for event in node:
    ///    match event["type"]:
    ///        case "INPUT":
    ///            match event["id"]:
    ///                 case "image":
    /// ```
    ///
    /// Default behaviour is to timeout after 2 seconds.
    ///
    /// :rtype: dict
    pub fn __next__(&mut self, py: Python) -> PyResult<Option<Py<PyDict>>> {
        self.next(py, None)
    }

    /// You can iterate over the event stream with a loop
    ///
    /// ```python
    /// for event in node:
    ///    match event["type"]:
    ///        case "INPUT":
    ///            match event["id"]:
    ///                 case "image":
    /// ```
    ///
    /// :rtype: dict
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// `send_output` send data from the node.
    ///
    /// ```python
    /// Args:
    ///    output_id: str,
    ///    data: pyarrow.Array,
    ///    metadata: Option[Dict],
    /// ```
    ///
    /// ex:
    ///
    /// ```python
    /// node.send_output("string", b"string", {"open_telemetry_context": "7632e76"})
    /// ```
    ///
    /// :type output_id: str
    /// :type data: pyarrow.Array
    /// :type metadata: dict, optional
    /// :rtype: None
    pub fn send_output(
        &mut self,
        output_id: String,
        data: PyObject,
        metadata: Option<Bound<'_, PyDict>>,
        py: Python,
    ) -> eyre::Result<()> {
        let parameters = pydict_to_metadata(metadata)?;

        if let Ok(py_bytes) = data.downcast_bound::<PyBytes>(py) {
            let data = py_bytes.as_bytes();
            self.node
                .get_mut()
                .send_output_bytes(output_id.into(), parameters, data.len(), data)
                .wrap_err("failed to send output")?;
        } else if let Ok(arrow_array) = arrow::array::ArrayData::from_pyarrow_bound(data.bind(py)) {
            self.node.get_mut().send_output(
                output_id.into(),
                parameters,
                arrow::array::make_array(arrow_array),
            )?;
        } else {
            eyre::bail!("invalid `data` type, must by `PyBytes` or arrow array")
        }

        Ok(())
    }

    /// Returns the full dataflow descriptor that this node is part of.
    ///
    /// This method returns the parsed dataflow YAML file.
    ///
    /// :rtype: dict
    pub fn dataflow_descriptor(&mut self, py: Python) -> eyre::Result<PyObject> {
        Ok(pythonize::pythonize(
            py,
            self.node.get_mut().dataflow_descriptor(),
        )?)
    }

    /// Returns the dataflow id.
    ///
    /// :rtype: str
    pub fn dataflow_id(&self) -> String {
        self.dataflow_id.to_string()
    }

    /// Merge an external event stream with dora main loop.
    /// This currently only work with ROS2.
    ///
    /// :type subscription: dora.Ros2Subscription
    /// :rtype: None
    pub fn merge_external_events(
        &mut self,
        subscription: &mut Ros2Subscription,
    ) -> eyre::Result<()> {
        let subscription = subscription.into_stream()?;
        let stream = futures::stream::poll_fn(move |cx| {
            let s = subscription.as_stream().map(|item| {
                match item.context("failed to read ROS2 message") {
                    Ok((value, _info)) => Python::with_gil(|py| {
                        value
                            .to_pyarrow(py)
                            .context("failed to convert value to pyarrow")
                            .unwrap_or_else(|err| PyErr::from(err).to_object(py))
                    }),
                    Err(err) => Python::with_gil(|py| PyErr::from(err).to_object(py)),
                }
            });
            futures::pin_mut!(s);
            s.poll_next_unpin(cx)
        });

        // take out the event stream and temporarily replace it with a dummy
        let events = std::mem::replace(
            &mut self.events.inner,
            EventsInner::Merged(Box::new(futures::stream::empty())),
        );
        // update self.events with the merged stream
        self.events.inner = EventsInner::Merged(events.merge_external_send(Box::pin(stream)));

        Ok(())
    }
}

struct Events {
    inner: EventsInner,
    cleanup_handle: NodeCleanupHandle,
}

impl Events {
    fn recv(&mut self, timeout: Option<Duration>) -> Option<PyEvent> {
        let event = match &mut self.inner {
            EventsInner::Dora(events) => match timeout {
                Some(timeout) => events
                    .get_mut()
                    .recv_timeout(timeout)
                    .map(MergedEvent::Dora),
                None => events.get_mut().recv().map(MergedEvent::Dora),
            },
            EventsInner::Merged(events) => futures::executor::block_on(events.next()),
        };
        event.map(|event| PyEvent {
            event,
            _cleanup: Some(self.cleanup_handle.clone()),
        })
    }
}

enum EventsInner {
    Dora(DelayedCleanup<EventStream>),
    Merged(Box<dyn Stream<Item = MergedEvent<PyObject>> + Unpin + Send>),
}

impl<'a> MergeExternalSend<'a, PyObject> for EventsInner {
    type Item = MergedEvent<PyObject>;

    fn merge_external_send(
        self,
        external_events: impl Stream<Item = PyObject> + Unpin + Send + 'a,
    ) -> Box<dyn Stream<Item = Self::Item> + Unpin + Send + 'a> {
        match self {
            EventsInner::Dora(events) => events.merge_external_send(external_events),
            EventsInner::Merged(events) => {
                let merged = events.merge_external_send(external_events);
                Box::new(merged.map(|event| match event {
                    MergedEvent::Dora(e) => MergedEvent::Dora(e),
                    MergedEvent::External(e) => MergedEvent::External(e.flatten()),
                }))
            }
        }
    }
}

impl Node {
    pub fn id(&self) -> String {
        self.node_id.to_string()
    }
}

/// Start a runtime for Operators
///
/// :rtype: None
#[pyfunction]
pub fn start_runtime() -> eyre::Result<()> {
    dora_runtime::main().wrap_err("Dora Runtime raised an error.")
}

#[pymodule]
fn dora(_py: Python, m: Bound<'_, PyModule>) -> PyResult<()> {
    dora_ros2_bridge_python::create_dora_ros2_bridge_module(&m)?;

    m.add_function(wrap_pyfunction!(start_runtime, &m)?)?;
    m.add_class::<Node>()?;
    m.setattr("__version__", env!("CARGO_PKG_VERSION"))?;
    m.setattr("__author__", "Dora-rs Authors")?;

    Ok(())
}
