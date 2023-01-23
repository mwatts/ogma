use super::{BoundaryNode, File, Import};
use crate::prelude::*;
use petgraph::prelude::*;
use std::path::Path;

type Inner = StableGraph<Node, (), Directed, u32>;

type NodesList = Arc<[NodeIndex]>;

lazy_static::lazy_static! {
    static ref EMPTY: Arc<[NodeIndex]> = Arc::new([]);
}

#[derive(Debug)]
pub enum Node {
    Boundary { name: Str, exports: NodesList },
    Type { name: Str, imports: NodesList },
    Impl { name: Str, imports: NodesList },
}

#[derive(Debug)]
pub struct Partitions {
    root: NodeIndex,
    shell: NodeIndex,
    plugins: NodeIndex,

    graph: Inner,
}

impl Partitions {
    pub fn new() -> Self {
        let mut graph = Inner::new();
        let root = graph.add_node(Node::Boundary {
            name: "<root>".into(),
            exports: EMPTY.clone(),
        });
        let shell = graph.add_node(Node::Boundary {
            name: "<shell>".into(),
            exports: EMPTY.clone(),
        });
        let plugins = graph.add_node(Node::Boundary {
            name: "<plugins>".into(),
            exports: EMPTY.clone(),
        });

        Self {
            root,
            shell,
            plugins,
            graph,
        }
    }

    pub fn extend_root(mut self, fsmap: super::FsMap) -> Result<Self> {
        // Phase 1: Create the structure of the graph given the map
        //          - we avoid the looking at the imports/exports for now
        //          - since we destruct the File, the remaining parts need to be stored for later

        let mut exports_map = <HashMap<_, Vec<_>>>::default();
        let mut imports_col = Vec::new();

        let root = self.root;
        for (p, files) in fsmap {
            let bnd = self.get_or_create_boundary_path(&p, root)?;

            for file in files {
                // deconstruct the file
                let File {
                    doc: _,
                    directives,
                    types,
                    impls,
                    exprs: _,
                } = file;

                // deconstruct the imports and exports
                let (imports, exports) =
                    directives
                        .into_iter()
                        .fold((Vec::new(), Vec::new()), |(mut i, mut e), x| {
                            match x {
                                lang::parse::Directive::Import(x) => i.extend(x),
                                lang::parse::Directive::Export(x) => e.extend(x),
                                lang::parse::Directive::FailFast
                                | lang::parse::Directive::NoParallelise => (),
                            }
                            (i, e)
                        });

                // extend the export map with the listed exports from this file,
                // note that we do not check for names yet, despite knowing the defs
                if !exports.is_empty() {
                    exports_map.entry(bnd).or_default().extend(exports);
                }

                // construct a nodes list which imports will be mapped to
                let mut ns = Vec::with_capacity(types.len() + impls.len());

                for (n, _) in &types {
                    ns.push(self.add_type(bnd, n)?);
                }

                for (n, _) in &impls {
                    ns.push(self.add_impl(bnd, n)?);
                }

                if !ns.is_empty() && !imports.is_empty() {
                    imports_col.push((bnd, imports, ns));
                }
            }
        }

        // Phase 2: Populate the boundary exports list
        //          - This list will be required for the imports resolution
        //          - We also check that all exports exist
        for (bnd, exports) in exports_map {
            self.add_exports(bnd, exports)?;
        }

        // Phase 3: Populate each item's import list
        for (bnd, imports, nodes) in imports_col {
            self.add_imports(bnd, imports, nodes)?;
        }

        Ok(self)
    }

    fn get_or_create_boundary_path(&mut self, path: &Path, root: NodeIndex) -> Result<NodeIndex> {
        let mut a = root;
        for p in path.iter() {
            let b = self
                .graph
                .neighbors(root)
                .find(|&n| self.graph[n].eq_boundary(p));
            let b = match b {
                Some(b) => b,
                None => {
                    let name = p.to_str().map(Str::new).unwrap_or_default();

                    is_valid_part_name(&name)?;

                    let b = self.graph.add_node(Node::Boundary {
                        name,
                        exports: EMPTY.clone(),
                    });
                    self.graph.add_edge(a, b, ());
                    b
                }
            };

            a = b; // next node
        }

        Ok(a)
    }

    fn add_type<N: Into<Str>>(&mut self, parent: NodeIndex, name: N) -> Result<NodeIndex> {
        let name = name.into();
        let exists = self
            .graph
            .neighbors(parent)
            .any(|n| self.graph[n].eq_type(&name));

        if exists {
            return Err(item_already_defined("type", &name));
        }

        let x = self.graph.add_node(Node::Type {
            name,
            imports: EMPTY.clone(),
        });

        self.graph.add_edge(parent, x, ());

        Ok(x)
    }

    fn add_impl<N: Into<Str>>(&mut self, parent: NodeIndex, name: N) -> Result<NodeIndex> {
        let name = name.into();
        let exists = self
            .graph
            .neighbors(parent)
            .any(|n| self.graph[n].eq_impl(&name));

        if exists {
            return Err(item_already_defined("impl", &name));
        }

        let x = self.graph.add_node(Node::Impl {
            name,
            imports: EMPTY.clone(),
        });

        self.graph.add_edge(parent, x, ());

        Ok(x)
    }

    fn add_exports(&mut self, boundary: NodeIndex, exports: Vec<Tag>) -> Result<()> {
        if exports.is_empty() {
            return Ok(());
        }

        let mut xs = if let Node::Boundary { name: _, exports } = &self.graph[boundary] {
            exports.to_vec()
        } else {
            panic!("`boundary` is not a BoundaryNode");
        };

        // currently this is a very slow search
        // <https://github.com/kdr-aus/ogma/issues/184> should be able to speed this up
        for e in exports {
            let i = self
                .graph
                .neighbors(boundary)
                .filter(|&n| !self.graph[n].is_boundary())
                .find(|&n| self.graph[n].name().eq(e.str()))
                .ok_or_else(|| Error {
                    cat: err::Category::Definitions,
                    desc: format!("could not find export item '{e}'"),
                    traces: err::trace(&e, "exports here".to_string()),
                    ..Error::default()
                })?;

            xs.push(i);
        }

        xs.sort();
        xs.dedup();

        if let Node::Boundary { name: _, exports } = &mut self.graph[boundary] {
            *exports = Arc::from(xs);
        };

        Ok(())
    }

    fn add_imports(
        &mut self,
        boundary: NodeIndex,
        imports: Vec<Import>,
        to: Vec<NodeIndex>,
    ) -> Result<()> {
        if imports.is_empty() {
            return Ok(());
        }

        let mut xs = self.resolve_imports(boundary, imports.iter())?;

        xs.sort();
        xs.dedup();

        let xs = Arc::from(xs);

        for n in to {
            match &mut self.graph[n] {
                Node::Boundary { .. } => (),
                Node::Type { name: _, imports } => *imports = Arc::clone(&xs),
                Node::Impl { name: _, imports } => *imports = Arc::clone(&xs),
            }
        }

        Ok(())
    }

    fn resolve_imports<'a, I>(&self, from: NodeIndex, imports: I) -> Result<Vec<NodeIndex>>
    where
        I: IntoIterator<Item = &'a Import>,
    {
        imports
            .into_iter()
            .map(|import| self.resolve_import(from, import))
            .try_fold(Vec::new(), |mut v, x| {
                x.map(|x| {
                    v.extend(x);
                    v
                })
            })
    }

    fn resolve_import(
        &self,
        from: NodeIndex,
        import: &Import,
    ) -> Result<impl Iterator<Item = NodeIndex> + '_> {
        let Import { path, glob } = import;

        let bnd = self.find_boundary(BoundaryNode(from), path)?;

        Ok(self.glob_find(bnd, glob)?.map(|(x, _)| x))
    }

    pub fn find_boundary<'a, P>(&self, from: BoundaryNode, path: P) -> Result<BoundaryNode>
    where
        P: IntoIterator<Item = &'a Tag>,
    {
        let mut a = from.into();

        for p in path {
            match self
                .graph
                .neighbors(a)
                .find(|&n| self.graph[n].eq_boundary(p))
            {
                Some(b) => a = b,
                None => {
                    let mut e = Error {
                        cat: err::Category::Definitions,
                        desc: format!("{p} does not match any partitions"),
                        traces: err::trace(p, None),
                        help_msg: None,
                        hard: true,
                    };

                    let x = self
                        .fuzzy_find(a, p)
                        .filter_map(|(_, n)| n.is_boundary().then(|| n.name().as_str()))
                        .collect::<Vec<_>>();

                    if !x.is_empty() {
                        let m = "Did you mean any of these? ".to_string() + &x.join(", ");
                        e.help_msg = m.into();
                    }

                    return Err(e);
                }
            }
        }

        Ok(BoundaryNode(a))
    }

    fn fuzzy_find<P: AsRef<str>>(
        &self,
        parent: NodeIndex,
        n: P,
    ) -> impl Iterator<Item = (NodeIndex, &Node)> {
        let mut e = simsearch::SimSearch::new();
        for n in self.graph.neighbors(parent) {
            e.insert(n, self.graph[n].name())
        }

        e.search(n.as_ref())
            .into_iter()
            .map(|n| (n, &self.graph[n]))
    }

    pub fn glob_find(
        &self,
        parent: BoundaryNode,
        pattern: &Tag,
    ) -> Result<impl Iterator<Item = (NodeIndex, &Node)>> {
        let pat = globset::Glob::new(pattern.str())
            .map_err(|e| Error {
                cat: err::Category::Parsing,
                desc: "invalid glob pattern".into(),
                traces: err::trace(pattern, format!("{e}")),
                help_msg: None,
                hard: true,
            })?
            .compile_matcher();

        Ok(self.graph.neighbors(parent.into()).filter_map(move |n| {
            let node = &self.graph[n];
            pat.is_match(node.name().as_str()).then_some((n, node))
        }))
    }
}

impl Node {
    pub fn name(&self) -> &Str {
        match self {
            Node::Boundary { name, .. } => name,
            Node::Type { name, .. } => name,
            Node::Impl { name, .. } => name,
        }
    }

    pub fn is_boundary(&self) -> bool {
        matches!(self, Node::Boundary { .. })
    }

    pub fn is_type(&self) -> bool {
        matches!(self, Node::Type { .. })
    }

    pub fn is_impl(&self) -> bool {
        matches!(self, Node::Impl { .. })
    }

    pub fn eq_boundary<N: PartialEq<str> + ?Sized>(&self, name: &N) -> bool {
        if let Node::Boundary { name: n, .. } = self {
            name.eq(n.as_str())
        } else {
            false
        }
    }

    pub fn eq_type<N: PartialEq<str> + ?Sized>(&self, name: &N) -> bool {
        if let Node::Type { name: n, .. } = self {
            name.eq(n.as_str())
        } else {
            false
        }
    }

    pub fn eq_impl<N: PartialEq<str> + ?Sized>(&self, name: &N) -> bool {
        if let Node::Impl { name: n, .. } = self {
            name.eq(n.as_str())
        } else {
            false
        }
    }
}

fn item_already_defined(ty: &str, name: &str) -> Error {
    use crate::prelude::err::*;

    Error {
        cat: Category::Definitions,
        desc: format!("the {ty} '{name}' is already defined"),
        hard: true,
        ..Error::default()
    }
}

fn is_valid_part_name(name: &str) -> Result<()> {
    use err::*;

    if name.is_empty() {
        Err(Error {
            cat: Category::Definitions,
            desc: "partition boundary names cannot be empty".into(),
            ..Default::default()
        })
    } else {
        let mut cs = name.chars();

        let f = cs.next().unwrap();

        if f == '_' || f.is_ascii_alphabetic() {
            if cs.all(|c| c == '-' || c == '_' || c.is_ascii_alphanumeric()) {
                Ok(())
            } else {
                Err(Error {
                cat: Category::Definitions,
                desc: format!("partition name '{name}' is invalid, it contains a character outside of _,-,a-z,A-Z,0-9"),
                ..Default::default()
            })
            }
        } else {
            Err(Error {
                cat: Category::Definitions,
                desc: format!("partition name '{name}' is invalid, must start with _,a-z,A-Z"),
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! describe {
        ($part:expr => $nl:literal:{$($ns:tt)*} $el:literal:[$($es:tt)*]) => {{
            assert_eq!($part.graph.node_count(), $nl);
            assert_eq!($part.graph.edge_count(), $el);
            describe!(@node $part, $($ns)*);
            describe!(@edge $part, $($es)*);
        }};
        (@node $part:expr, $($idx:literal : $t:ident $name:literal,)*) => {{
            $(describe!(@node1 $t $part, $idx, $name);)*
        }};
        (@node1 B $part:expr, $idx:literal, $name:literal) => {{
            let n = $part.graph.node_weight(NodeIndex::from($idx))
                .expect(&format!("node with index {}", $idx));
            match n {
                Node::Boundary { name , .. } => assert_eq!(name, $name),
                x => panic!("expecting Boundary node at {}, found: {x:?}", $idx),
            };
        }};
        (@node1 T $part:expr, $idx:literal, $name:literal) => {{
            let n = $part.graph.node_weight(NodeIndex::from($idx))
                .expect(&format!("node with index {}", $idx));
            match n {
                Node::Type { name , .. } => assert_eq!(name, $name),
                x => panic!("expecting Type node at {}, found: {x:?}", $idx),
            };
        }};
        (@node1 I $part:expr, $idx:literal, $name:literal) => {{
            let n = $part.graph.node_weight(NodeIndex::from($idx))
                .expect(&format!("node with index {}", $idx));
            match n {
                Node::Impl { name , .. } => assert_eq!(name, $name),
                x => panic!("expecting Boundary node at {}, found: {x:?}", $idx),
            };
        }};
        (@edge $part:expr, $($a:literal -> $b:literal ,)*) => {{
            $(
                assert!($part.graph.contains_edge(
                    NodeIndex::from($a), NodeIndex::from($b)));
            )*
        }};
    }

    fn mkmap<I>(iter: I) -> super::super::FsMap
    where
        I: IntoIterator<
            Item = (
                &'static str,
                Vec<std::result::Result<&'static str, &'static str>>,
            ),
        >,
    {
        use lang::parse::Item;
        iter.into_iter()
            .map(|(p, f)| {
                let f = f
                    .into_iter()
                    .map(|f| {
                        let mut file = lang::parse::File::empty();
                        match f {
                            Ok(x) => file.types.push((x.to_string(), Item::dummy())),
                            Err(x) => file.impls.push((x.to_string(), Item::dummy())),
                        }
                        file
                    })
                    .collect();
                (std::path::PathBuf::from(p), f)
            })
            .collect()
    }

    #[test]
    fn extending_partition_graph_with_fsmap() {
        let p = Partitions::new();

        describe! { p =>
            3:{0: B "<root>", 1: B "<shell>", 2: B "<plugins>",}
            0:[]
        };

        let p = p.extend_root(Default::default()).unwrap();

        describe! { p =>
            3:{0: B "<root>", 1: B "<shell>", 2: B "<plugins>",}
            0:[]
        };

        let p = p.extend_root(mkmap([("foo", vec![])])).unwrap();

        describe! { p =>
            4:{0: B "<root>", 1: B "<shell>", 2: B "<plugins>", 3: B "foo",}
            1:[0 -> 3,]
        };

        let p = p
            .extend_root(mkmap([("foo", vec![Ok("TypeA"), Err("impl-a")])]))
            .unwrap();

        describe! { p =>
            6:{0: B "<root>", 1: B "<shell>", 2: B "<plugins>", 3: B "foo",
               4: T "TypeA", 5: I "impl-a", }
            3:[0->3, 3->4, 3->5,]
        };

        let p = p
            .extend_root(mkmap([("foo/bar/zog", vec![Ok("TypeA"), Err("impl-a")])]))
            .unwrap();

        describe! { p =>
            10:{0: B "<root>", 1: B "<shell>", 2: B "<plugins>", 3: B "foo",
                4: T "TypeA", 5: I "impl-a",
                6: B "bar", 7: B "zog", 8: T "TypeA", 9: I "impl-a",}
            7:[0->3,3->4,3->5,3->6,6->7,7->8,7->9,]
        };
    }

    #[test]
    fn extending_fails() {
        let x = Partitions::new()
            .extend_root(mkmap([("foo", vec![Ok("TypeA"), Ok("TypeA")])]))
            .unwrap_err();

        assert_eq!(
            &x.to_string(),
            "Definition Error: the type 'TypeA' is already defined
"
        );

        let x = Partitions::new()
            .extend_root(mkmap([
                ("foo", vec![Err("impl-a")]),
                ("foo/bar", vec![Ok("TypeA")]),
            ]))
            .unwrap()
            .extend_root(mkmap([("foo", vec![Err("impl-b"), Err("impl-a")])]))
            .unwrap_err();

        assert_eq!(
            &x.to_string(),
            "Definition Error: the impl 'impl-a' is already defined
"
        );
    }

    #[test]
    fn valid_partition_naming() {
        // empty path does not iterate
        assert!(Partitions::new().extend_root(mkmap([("", vec![])])).is_ok());

        let x = Partitions::new()
            .extend_root(mkmap([("🌏", vec![])]))
            .unwrap_err();

        assert_eq!(
            &x.to_string(),
            "Definition Error: partition name '🌏' is invalid, must start with _,a-z,A-Z
"
        );

        let x = Partitions::new()
            .extend_root(mkmap([(" foo", vec![])]))
            .unwrap_err();

        assert_eq!(
            &x.to_string(),
            "Definition Error: partition name ' foo' is invalid, must start with _,a-z,A-Z
"
        );

        let x = Partitions::new()
            .extend_root(mkmap([("1234", vec![])]))
            .unwrap_err();

        assert_eq!(
            &x.to_string(),
            "Definition Error: partition name '1234' is invalid, must start with _,a-z,A-Z
"
        );

        let x = Partitions::new()
            .extend_root(mkmap([("Hello, 🌏", vec![])]))
            .unwrap_err();

        assert_eq!(&x.to_string(), "Definition Error: partition name 'Hello, 🌏' is invalid, it contains a character outside of _,-,a-z,A-Z,0-9
");
    }
}
