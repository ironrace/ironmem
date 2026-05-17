# Python labeler §9.1 spot-check — annotated review

Source: `benchmarks/provbench/results/python-labeler-2026-05-15-spotcheck.csv` (200 rows)
Repo: `pallets/flask` at T₀ `2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (tag 2.0.0)

**Review protocol:** scan each row's source context (`→` marks the fact line). Note any **disagreements** — rows where the labeler incorrectly identified the fact (wrong qualified name, wrong line, wrong kind, or extracted a non-fact). Reply with a list of row numbers (`row #N`) + a short note per disagreement; I'll patch the CSV in bulk and run `provbench-labeler report` for the Wilson 95% LB.

**Counts by kind:** Field=8, FunctionSignature=74, PublicSymbol=37, TestAssertion=81

---

## Field (8 rows)

### row #1 — `src.flask.app.Flask.secret_key` @ `src/flask/app.py:255`

```python
   252      #:
   253      #: This attribute can also be configured from the config with the
   254      #: :data:`SECRET_KEY` configuration key. Defaults to ``None``.
→  255      secret_key = ConfigAttribute("SECRET_KEY")
   256  
   257      #: The secure cookie uses this for the name of the session cookie.
   258      #:
```

### row #2 — `src.flask.json.tag.JSONTag.key` @ `src/flask/json/tag.py:64`

```python
    61  
    62      #: The tag to mark the serialized object with. If ``None``, this tag is
    63      #: only used as an intermediate step during tagging.
→   64      key: t.Optional[str] = None
    65  
    66      def __init__(self, serializer: "TaggedJSONSerializer") -> None:
    67          """Create a tagger for the given serializer."""
```

### row #3 — `src.flask.json.tag.TagBytes.__slots__` @ `src/flask/json/tag.py:157`

```python
   154  
   155  
   156  class TagBytes(JSONTag):
→  157      __slots__ = ()
   158      key = " b"
   159  
   160      def check(self, value: t.Any) -> bool:
```

### row #4 — `src.flask.json.tag.TaggedJSONSerializer.default_tags` @ `src/flask/json/tag.py:235`

```python
   232  
   233      #: Tag classes to bind when creating the serializer. Other tags can be
   234      #: added later using :meth:`~register`.
→  235      default_tags = [
   236          TagDict,
   237          PassDict,
   238          TagTuple,
```

### row #5 — `src.flask.sessions.SecureCookieSessionInterface.serializer` @ `src/flask/sessions.py:331`

```python
   328      #: A python serializer for the payload.  The default is a compact
   329      #: JSON derived serializer with support for some extra Python types
   330      #: such as datetime objects or tuples.
→  331      serializer = session_json_serializer
   332      session_class = SecureCookieSession
   333  
   334      def get_signing_serializer(
```

### row #6 — `src.flask.sessions.SessionMixin.modified` @ `src/flask/sessions.py:39`

```python
    36      #: Some implementations can detect changes to the session and set
    37      #: this when that happens. The mixin default is hard coded to
    38      #: ``True``.
→   39      modified = True
    40  
    41      #: Some implementations can detect when session data is read or
    42      #: written and set this when that happens. The mixin default is hard
```

### row #7 — `src.flask.signals._FakeSignal.disconnect` @ `src/flask/signals.py:35`

```python
    32              )
    33  
    34          connect = connect_via = connected_to = temporarily_connected_to = _fail
→   35          disconnect = _fail
    36          has_receivers_for = receivers_for = _fail
    37          del _fail
    38  
```

### row #8 — `tests.conftest.Flask.testing` @ `tests/conftest.py:47`

```python
    44  
    45  
    46  class Flask(_Flask):
→   47      testing = True
    48      secret_key = "test key"
    49  
    50  
```

## FunctionSignature (74 rows)

### row #9 — `examples.tutorial.tests.test_blog.test_author_required` @ `examples/tutorial/tests/test_blog.py:25`

```python
    22      assert response.headers["Location"] == "http://localhost/auth/login"
    23  
    24  
→   25  def test_author_required(app, client, auth):
    26      # change the post author to another user
    27      with app.app_context():
    28          db = get_db()
```

### row #10 — `src.flask.app.Flask._is_setup_finished` @ `src/flask/app.py:523`

```python
   520          # the app's commands to another CLI tool.
   521          self.cli.name = self.name
   522  
→  523      def _is_setup_finished(self) -> bool:
   524          return self.debug and self._got_first_request
   525  
   526      @locked_cached_property
```

### row #11 — `src.flask.app.Flask.add_url_rule` @ `src/flask/app.py:1032`

```python
  1029          return self.blueprints.values()
  1030  
  1031      @setupmethod
→ 1032      def add_url_rule(
  1033          self,
  1034          rule: str,
  1035          endpoint: t.Optional[str] = None,
```

### row #12 — `src.flask.app.Flask.ensure_sync` @ `src/flask/app.py:1572`

```python
  1569          """
  1570          return False
  1571  
→ 1572      def ensure_sync(self, func: t.Callable) -> t.Callable:
  1573          """Ensure that the function is synchronous for WSGI workers.
  1574          Plain ``def`` functions are returned as-is. ``async def``
  1575          functions are wrapped to run and wait for the response.
```

### row #13 — `src.flask.app.Flask.handle_http_exception` @ `src/flask/app.py:1276`

```python
  1273                          return handler
  1274          return None
  1275  
→ 1276      def handle_http_exception(
  1277          self, e: HTTPException
  1278      ) -> t.Union[HTTPException, ResponseReturnValue]:
  1279          """Handles an HTTP exception.  By default this will invoke the
```

### row #14 — `src.flask.app.Flask.handle_url_build_error` @ `src/flask/app.py:1790`

```python
  1787          for func in funcs:
  1788              func(endpoint, values)
  1789  
→ 1790      def handle_url_build_error(
  1791          self, error: Exception, endpoint: str, values: dict
  1792      ) -> str:
  1793          """Handle :class:`~werkzeug.routing.BuildError` on
```

### row #15 — `src.flask.app.Flask.logger` @ `src/flask/app.py:569`

```python
   566          return self.debug
   567  
   568      @locked_cached_property
→  569      def logger(self) -> logging.Logger:
   570          """A standard Python :class:`~logging.Logger` for the app, with
   571          the same name as :attr:`name`.
   572  
```

### row #16 — `src.flask.app.Flask.preprocess_request` @ `src/flask/app.py:1813`

```python
  1810  
  1811          raise error
  1812  
→ 1813      def preprocess_request(self) -> t.Optional[ResponseReturnValue]:
  1814          """Called before the request is dispatched. Calls
  1815          :attr:`url_value_preprocessors` registered with the app and the
  1816          current blueprint (if any). Then calls :attr:`before_request_funcs`
```

### row #17 — `src.flask.app.Flask.template_global` @ `src/flask/app.py:1164`

```python
  1161          self.jinja_env.tests[name or f.__name__] = f
  1162  
  1163      @setupmethod
→ 1164      def template_global(self, name: t.Optional[str] = None) -> t.Callable:
  1165          """A decorator that is used to register a custom template global function.
  1166          You can specify a name for the global function, otherwise the function
  1167          name will be used. Example::
```

### row #18 — `src.flask.app.Flask.test_request_context` @ `src/flask/app.py:1965`

```python
  1962          """
  1963          return RequestContext(self, environ)
  1964  
→ 1965      def test_request_context(self, *args: t.Any, **kwargs: t.Any) -> RequestContext:
  1966          """Create a :class:`~flask.ctx.RequestContext` for a WSGI
  1967          environment created from the given values. This is mostly useful
  1968          during testing, where you may want to run a function that uses
```

### row #19 — `src.flask.blueprints.BlueprintSetupState.__init__` @ `src/flask/blueprints.py:32`

```python
    29      to all register callback functions.
    30      """
    31  
→   32      def __init__(
    33          self,
    34          blueprint: "Blueprint",
    35          app: "Flask",
```

### row #20 — `src.flask.ctx.AppContext.pop` @ `src/flask/ctx.py:225`

```python
   222          _app_ctx_stack.push(self)
   223          appcontext_pushed.send(self.app)
   224  
→  225      def pop(self, exc: t.Optional[BaseException] = _sentinel) -> None:  # type: ignore
   226          """Pops the app context."""
   227          try:
   228              self._refcnt -= 1
```

### row #21 — `src.flask.ctx.RequestContext.__exit__` @ `src/flask/ctx.py:446`

```python
   443          self.push()
   444          return self
   445  
→  446      def __exit__(
   447          self, exc_type: type, exc_value: BaseException, tb: TracebackType
   448      ) -> None:
   449          # do not pop the request stack if we are in debug mode and an
```

### row #22 — `src.flask.ctx.RequestContext.__init__` @ `src/flask/ctx.py:278`

```python
   275      that situation, otherwise your unittests will leak memory.
   276      """
   277  
→  278      def __init__(
   279          self,
   280          app: "Flask",
   281          environ: dict,
```

### row #23 — `src.flask.ctx.RequestContext.g` @ `src/flask/ctx.py:317`

```python
   314          self._after_request_functions: t.List[AfterRequestCallable] = []
   315  
   316      @property
→  317      def g(self) -> AppContext:
   318          return _app_ctx_stack.top.g
   319  
   320      @g.setter
```

### row #24 — `src.flask.debughelpers._dump_loader_info` @ `src/flask/debughelpers.py:96`

```python
    93      request.files.__class__ = newcls
    94  
    95  
→   96  def _dump_loader_info(loader) -> t.Generator:
    97      yield f"class: {type(loader).__module__}.{type(loader).__name__}"
    98      for key, value in sorted(loader.__dict__.items()):
    99          if key.startswith("_"):
```

### row #25 — `src.flask.globals._lookup_app_object` @ `src/flask/globals.py:37`

```python
    34      return getattr(top, name)
    35  
    36  
→   37  def _lookup_app_object(name):
    38      top = _app_ctx_stack.top
    39      if top is None:
    40          raise RuntimeError(_app_ctx_err_msg)
```

### row #26 — `src.flask.helpers.get_debug_flag` @ `src/flask/helpers.py:35`

```python
    32      return os.environ.get("FLASK_ENV") or "production"
    33  
    34  
→   35  def get_debug_flag() -> bool:
    36      """Get whether debug mode should be enabled for the app, indicated
    37      by the :envvar:`FLASK_DEBUG` environment variable. The default is
    38      ``True`` if :func:`.get_env` returns ``'development'``, or ``False``
```

### row #27 — `src.flask.helpers.get_env` @ `src/flask/helpers.py:27`

```python
    24      from .wrappers import Response
    25  
    26  
→   27  def get_env() -> str:
    28      """Get the environment the app is running in, indicated by the
    29      :envvar:`FLASK_ENV` environment variable. The default is
    30      ``'production'``.
```

### row #28 — `src.flask.helpers.get_template_attribute` @ `src/flask/helpers.py:341`

```python
   338      return rv
   339  
   340  
→  341  def get_template_attribute(template_name: str, attribute: str) -> t.Any:
   342      """Loads a macro (or variable) a template exports.  This can be used to
   343      invoke a macro from within Python code.  If you for example have a
   344      template named :file:`_cider.html` with the following contents:
```

### row #29 — `src.flask.helpers.is_ip` @ `src/flask/helpers.py:784`

```python
   781      return td.days * 60 * 60 * 24 + td.seconds
   782  
   783  
→  784  def is_ip(value: str) -> bool:
   785      """Determine if the given string is an IP address.
   786  
   787      :param value: value to check
```

### row #30 — `src.flask.helpers.send_from_directory` @ `src/flask/helpers.py:645`

```python
   642      return path
   643  
   644  
→  645  def send_from_directory(directory: str, path: str, **kwargs: t.Any) -> "Response":
   646      """Send a file from within a directory using :func:`send_file`.
   647  
   648      .. code-block:: python
```

### row #31 — `src.flask.json.tag.TagBytes.to_json` @ `src/flask/json/tag.py:163`

```python
   160      def check(self, value: t.Any) -> bool:
   161          return isinstance(value, bytes)
   162  
→  163      def to_json(self, value: t.Any) -> t.Any:
   164          return b64encode(value).decode("ascii")
   165  
   166      def to_python(self, value: t.Any) -> t.Any:
```

### row #32 — `src.flask.scaffold.Scaffold.__init__` @ `src/flask/scaffold.py:89`

```python
    86      #: blueprint sets this, it will be used instead of the app's value.
    87      json_decoder: t.Optional[t.Type[JSONDecoder]] = None
    88  
→   89      def __init__(
    90          self,
    91          import_name: str,
    92          static_folder: t.Optional[str] = None,
```

### row #33 — `src.flask.scaffold.Scaffold.errorhandler` @ `src/flask/scaffold.py:643`

```python
   640          return f
   641  
   642      @setupmethod
→  643      def errorhandler(
   644          self, code_or_exception: t.Union[t.Type[Exception], int]
   645      ) -> t.Callable:
   646          """Register a function to handle errors by code or exception class.
```

### row #34 — `src.flask.scaffold.Scaffold.get` @ `src/flask/scaffold.py:372`

```python
   369  
   370          return self.route(rule, methods=[method], **options)
   371  
→  372      def get(self, rule: str, **options: t.Any) -> t.Callable:
   373          """Shortcut for :meth:`route` with ``methods=["GET"]``.
   374  
   375          .. versionadded:: 2.0
```

### row #35 — `src.flask.scaffold.Scaffold.patch` @ `src/flask/scaffold.py:400`

```python
   397          """
   398          return self._method_route("DELETE", rule, options)
   399  
→  400      def patch(self, rule: str, **options: t.Any) -> t.Callable:
   401          """Shortcut for :meth:`route` with ``methods=["PATCH"]``.
   402  
   403          .. versionadded:: 2.0
```

### row #36 — `src.flask.scaffold.Scaffold.url_defaults` @ `src/flask/scaffold.py:634`

```python
   631          return f
   632  
   633      @setupmethod
→  634      def url_defaults(self, f: URLDefaultCallable) -> URLDefaultCallable:
   635          """Callback function for URL defaults for all view functions of the
   636          application.  It's called with the endpoint and values and should
   637          update the values passed in place.
```

### row #37 — `src.flask.sessions.SecureCookieSessionInterface.save_session` @ `src/flask/sessions.py:365`

```python
   362          except BadSignature:
   363              return self.session_class()
   364  
→  365      def save_session(
   366          self, app: "Flask", session: SessionMixin, response: "Response"
   367      ) -> None:
   368          name = self.get_cookie_name(app)
```

### row #38 — `src.flask.sessions.SessionInterface.get_cookie_httponly` @ `src/flask/sessions.py:243`

```python
   240          """
   241          return app.config["SESSION_COOKIE_PATH"] or app.config["APPLICATION_ROOT"]
   242  
→  243      def get_cookie_httponly(self, app: "Flask") -> bool:
   244          """Returns True if the session cookie should be httponly.  This
   245          currently just returns the value of the ``SESSION_COOKIE_HTTPONLY``
   246          config var.
```

### row #39 — `src.flask.sessions.SessionInterface.get_cookie_name` @ `src/flask/sessions.py:170`

```python
   167          """
   168          return isinstance(obj, self.null_session_class)
   169  
→  170      def get_cookie_name(self, app: "Flask") -> str:
   171          """Returns the name of the session cookie.
   172  
   173          Uses ``app.session_cookie_name`` which is set to ``SESSION_COOKIE_NAME``
```

### row #40 — `src.flask.sessions.SessionInterface.make_null_session` @ `src/flask/sessions.py:149`

```python
   146      #: .. versionadded:: 0.10
   147      pickle_based = False
   148  
→  149      def make_null_session(self, app: "Flask") -> NullSession:
   150          """Creates a null session which acts as a replacement object if the
   151          real session support could not be loaded due to a configuration
   152          error.  This mainly aids the user experience because the job of the
```

### row #41 — `src.flask.templating.DispatchingJinjaLoader._iter_loaders` @ `src/flask/templating.py:97`

```python
    94                  continue
    95          raise TemplateNotFound(template)
    96  
→   97      def _iter_loaders(
    98          self, template: str
    99      ) -> t.Generator[t.Tuple["Scaffold", BaseLoader], None, None]:
   100          loader = self.app.jinja_loader
```

### row #42 — `tests.test_appctx.test_app_tearing_down_with_unhandled_exception` @ `tests/test_appctx.py:111`

```python
   108      assert cleanup_stuff == [None]
   109  
   110  
→  111  def test_app_tearing_down_with_unhandled_exception(app, client):
   112      app.config["PROPAGATE_EXCEPTIONS"] = True
   113      cleanup_stuff = []
   114  
```

### row #43 — `tests.test_appctx.test_basic_url_generation` @ `tests/test_appctx.py:6`

```python
     3  import flask
     4  
     5  
→    6  def test_basic_url_generation(app):
     7      app.config["SERVER_NAME"] = "localhost"
     8      app.config["PREFERRED_URL_SCHEME"] = "https"
     9  
```

### row #44 — `tests.test_appctx.test_request_context_means_app_context` @ `tests/test_appctx.py:30`

```python
    27          flask.url_for("index")
    28  
    29  
→   30  def test_request_context_means_app_context(app):
    31      with app.test_request_context():
    32          assert flask.current_app._get_current_object() == app
    33      assert flask._app_ctx_stack.top is None
```

### row #45 — `tests.test_apps.cliapp.factory.create_app2` @ `tests/test_apps/cliapp/factory.py:8`

```python
     5      return Flask("app")
     6  
     7  
→    8  def create_app2(foo, bar):
     9      return Flask("_".join(["app2", foo, bar]))
    10  
    11  
```

### row #46 — `tests.test_async.test_async_error_handler` @ `tests/test_async.py:71`

```python
    68  
    69  @pytest.mark.skipif(sys.version_info < (3, 7), reason="requires Python >= 3.7")
    70  @pytest.mark.parametrize("path", ["/error", "/bp/error"])
→   71  def test_async_error_handler(path, async_app):
    72      test_client = async_app.test_client()
    73      response = test_client.get(path)
    74      assert response.status_code == 412
```

### row #47 — `tests.test_async.test_async_route` @ `tests/test_async.py:61`

```python
    58  
    59  @pytest.mark.skipif(sys.version_info < (3, 7), reason="requires Python >= 3.7")
    60  @pytest.mark.parametrize("path", ["/", "/home", "/bp/"])
→   61  def test_async_route(path, async_app):
    62      test_client = async_app.test_client()
    63      response = test_client.get(path)
    64      assert b"GET" in response.get_data()
```

### row #48 — `tests.test_basic.test_debug_mode_complains_after_first_request` @ `tests/test_basic.py:1666`

```python
  1663      assert rv.data == b"Hello World!"
  1664  
  1665  
→ 1666  def test_debug_mode_complains_after_first_request(app, client):
  1667      app.debug = True
  1668  
  1669      @app.route("/")
```

### row #49 — `tests.test_basic.test_error_handling` @ `tests/test_basic.py:866`

```python
   863      assert called == [1, 2, 3, 4, 5, 6]
   864  
   865  
→  866  def test_error_handling(app, client):
   867      app.testing = False
   868  
   869      @app.errorhandler(404)
```

### row #50 — `tests.test_basic.test_jsonify_prettyprint` @ `tests/test_basic.py:1308`

```python
  1305      assert rv.data == compressed_msg
  1306  
  1307  
→ 1308  def test_jsonify_prettyprint(app, req_ctx):
  1309      app.config.update({"JSONIFY_PRETTYPRINT_REGULAR": True})
  1310      compressed_msg = {"msg": {"submsg": "W00t"}, "msg2": "foobar"}
  1311      pretty_response = (
```

### row #51 — `tests.test_basic.test_preserve_remembers_exception` @ `tests/test_basic.py:1794`

```python
  1791      assert flask._app_ctx_stack.top is None
  1792  
  1793  
→ 1794  def test_preserve_remembers_exception(app, client):
  1795      app.debug = True
  1796      errors = []
  1797  
```

### row #52 — `tests.test_basic.test_werkzeug_passthrough_errors` @ `tests/test_basic.py:1569`

```python
  1566  @pytest.mark.parametrize("use_debugger", [True, False])
  1567  @pytest.mark.parametrize("use_reloader", [True, False])
  1568  @pytest.mark.parametrize("propagate_exceptions", [None, True, False])
→ 1569  def test_werkzeug_passthrough_errors(
  1570      monkeypatch, debug, use_debugger, use_reloader, propagate_exceptions, app
  1571  ):
  1572      rv = {}
```

### row #53 — `tests.test_blueprints.test_add_template_filter` @ `tests/test_blueprints.py:434`

```python
   431      assert app.jinja_env.filters["my_reverse"]("abcd") == "dcba"
   432  
   433  
→  434  def test_add_template_filter(app):
   435      bp = flask.Blueprint("bp", __name__)
   436  
   437      def my_reverse(s):
```

### row #54 — `tests.test_blueprints.test_add_template_test_with_name` @ `tests/test_blueprints.py:596`

```python
   593      assert app.jinja_env.tests["boolean"](False)
   594  
   595  
→  596  def test_add_template_test_with_name(app):
   597      bp = flask.Blueprint("bp", __name__)
   598  
   599      def is_boolean(value):
```

### row #55 — `tests.test_blueprints.test_blueprint_app_error_handling` @ `tests/test_blueprints.py:82`

```python
    79      assert client.get("/function").data == b"bam"
    80  
    81  
→   82  def test_blueprint_app_error_handling(app, client):
    83      errors = flask.Blueprint("errors", __name__)
    84  
    85      @errors.app_errorhandler(403)
```

### row #56 — `tests.test_blueprints.test_template_filter_with_template` @ `tests/test_blueprints.py:473`

```python
   470      assert app.jinja_env.filters["strrev"]("abcd") == "dcba"
   471  
   472  
→  473  def test_template_filter_with_template(app, client):
   474      bp = flask.Blueprint("bp", __name__)
   475  
   476      @bp.app_template_filter()
```

### row #57 — `tests.test_cli.test_dotenv_optional` @ `tests/test_cli.py:542`

```python
   539      assert "FOO" in os.environ
   540  
   541  
→  542  def test_dotenv_optional(monkeypatch):
   543      monkeypatch.setattr("flask.cli.dotenv", None)
   544      monkeypatch.chdir(test_path)
   545      load_dotenv()
```

### row #58 — `tests.test_cli.test_flaskgroup_debug` @ `tests/test_cli.py:382`

```python
   379  
   380  
   381  @pytest.mark.parametrize("set_debug_flag", (True, False))
→  382  def test_flaskgroup_debug(runner, set_debug_flag):
   383      def create_app():
   384          app = Flask("flaskgroup")
   385          app.debug = True
```

### row #59 — `tests.test_cli.test_get_version` @ `tests/test_cli.py:254`

```python
   251          locate_app(info, "cliapp.importerrorapp", None, raise_if_not_found=False)
   252  
   253  
→  254  def test_get_version(test_apps, capsys):
   255      from flask import __version__ as flask_version
   256      from werkzeug import __version__ as werkzeug_version
   257      from platform import python_version
```

### row #60 — `tests.test_cli.test_help_echo_loading_error` @ `tests/test_cli.py:411`

```python
   408      assert "Usage:" in result.stderr
   409  
   410  
→  411  def test_help_echo_loading_error():
   412      from flask.cli import cli
   413  
   414      runner = CliRunner(mix_stderr=False)
```

### row #61 — `tests.test_cli.test_locate_app_suppress_raise` @ `tests/test_cli.py:244`

```python
   241          locate_app(info, iname, aname)
   242  
   243  
→  244  def test_locate_app_suppress_raise(test_apps):
   245      info = ScriptInfo()
   246      app = locate_app(info, "notanapp.py", None, raise_if_not_found=False)
   247      assert app is None
```

### row #62 — `tests.test_cli.test_scriptinfo` @ `tests/test_cli.py:274`

```python
   271      assert f"Werkzeug {werkzeug_version}" in out
   272  
   273  
→  274  def test_scriptinfo(test_apps, monkeypatch):
   275      obj = ScriptInfo(app_import_path="cliapp.app:testapp")
   276      app = obj.load_app()
   277      assert app.name == "testapp"
```

### row #63 — `tests.test_config.test_config_from_envvar` @ `tests/test_config.py:71`

```python
    68      common_object_test(app)
    69  
    70  
→   71  def test_config_from_envvar(monkeypatch):
    72      monkeypatch.setattr("os.environ", {})
    73      app = flask.Flask(__name__)
    74      with pytest.raises(RuntimeError) as e:
```

### row #64 — `tests.test_config.test_config_from_object` @ `tests/test_config.py:28`

```python
    25      common_object_test(app)
    26  
    27  
→   28  def test_config_from_object():
    29      app = flask.Flask(__name__)
    30      app.config.from_object(__name__)
    31      common_object_test(app)
```

### row #65 — `tests.test_config.test_session_lifetime` @ `tests/test_config.py:136`

```python
   133      common_object_test(app)
   134  
   135  
→  136  def test_session_lifetime():
   137      app = flask.Flask(__name__)
   138      app.config["PERMANENT_SESSION_LIFETIME"] = 42
   139      assert app.permanent_session_lifetime.seconds == 42
```

### row #66 — `tests.test_helpers.FakePath.__fspath__` @ `tests/test_helpers.py:21`

```python
    18      def __init__(self, path):
    19          self.path = path
    20  
→   21      def __fspath__(self):
    22          return self.path
    23  
    24  
```

### row #67 — `tests.test_helpers.TestStreaming.test_streaming_with_context_as_decorator` @ `tests/test_helpers.py:194`

```python
   191          rv = client.get("/?name=World")
   192          assert rv.data == b"Hello World!"
   193  
→  194      def test_streaming_with_context_as_decorator(self, app, client):
   195          @app.route("/")
   196          def index():
   197              @flask.stream_with_context
```

### row #68 — `tests.test_json.test_json_customization` @ `tests/test_json.py:217`

```python
   214      )
   215  
   216  
→  217  def test_json_customization(app, client):
   218      class X:  # noqa: B903, for Python2 compatibility
   219          def __init__(self, val):
   220              self.val = val
```

### row #69 — `tests.test_json_tag.test_dump_load_unchanged` @ `tests/test_json_tag.py:27`

```python
    24          datetime.now(tz=timezone.utc).replace(microsecond=0),
    25      ),
    26  )
→   27  def test_dump_load_unchanged(data):
    28      s = TaggedJSONSerializer()
    29      assert s.loads(s.dumps(data)) == data
    30  
```

### row #70 — `tests.test_reqctx.test_proper_test_request_context` @ `tests/test_reqctx.py:60`

```python
    57      assert buffer == [None]
    58  
    59  
→   60  def test_proper_test_request_context(app):
    61      app.config.update(SERVER_NAME="localhost.localdomain:5000")
    62  
    63      @app.route("/")
```

### row #71 — `tests.test_reqctx.test_session_dynamic_cookie_name` @ `tests/test_reqctx.py:223`

```python
   220      assert not flask.current_app
   221  
   222  
→  223  def test_session_dynamic_cookie_name():
   224  
   225      # This session interface will use a cookie with a different name if the
   226      # requested url ends with the string "dynamic_cookie"
```

### row #72 — `tests.test_signals.test_appcontext_tearing_down_signal` @ `tests/test_signals.py:177`

```python
   174          flask.message_flashed.disconnect(record, app)
   175  
   176  
→  177  def test_appcontext_tearing_down_signal():
   178      app = flask.Flask(__name__)
   179      recorded = []
   180  
```

### row #73 — `tests.test_templating.test_add_template_filter` @ `tests/test_templating.py:123`

```python
   120      assert app.jinja_env.filters["my_reverse"]("abcd") == "dcba"
   121  
   122  
→  123  def test_add_template_filter(app):
   124      def my_reverse(s):
   125          return s[::-1]
   126  
```

### row #74 — `tests.test_templating.test_custom_jinja_env` @ `tests/test_templating.py:435`

```python
   432      assert len(called) == 1
   433  
   434  
→  435  def test_custom_jinja_env():
   436      class CustomEnvironment(flask.templating.Environment):
   437          pass
   438  
```

### row #75 — `tests.test_templating.test_original_win` @ `tests/test_templating.py:23`

```python
    20      assert rv.data == b"<p>23|42"
    21  
    22  
→   23  def test_original_win(app, client):
    24      @app.route("/")
    25      def index():
    26          return flask.render_template_string("{{ config }}", config=42)
```

### row #76 — `tests.test_testing.test_session_transaction_needs_cookies` @ `tests/test_testing.py:207`

```python
   204          assert req is flask.request._get_current_object()
   205  
   206  
→  207  def test_session_transaction_needs_cookies(app):
   208      c = app.test_client(use_cookies=False)
   209      with pytest.raises(RuntimeError) as e:
   210          with c.session_transaction():
```

### row #77 — `tests.test_testing.test_test_client_calls_teardown_handlers` @ `tests/test_testing.py:257`

```python
   254          assert client.get("/").status_code == 404
   255  
   256  
→  257  def test_test_client_calls_teardown_handlers(app, client):
   258      called = []
   259  
   260      @app.teardown_request
```

### row #78 — `tests.test_testing.test_test_client_context_binding` @ `tests/test_testing.py:215`

```python
   212      assert "cookies" in str(e.value)
   213  
   214  
→  215  def test_test_client_context_binding(app, client):
   216      app.testing = False
   217  
   218      @app.route("/")
```

### row #79 — `tests.test_user_error_handler.TestGenericHandlers.report_error` @ `tests/test_user_error_handler.py:238`

```python
   235          app.config["PROPAGATE_EXCEPTIONS"] = False
   236          return app
   237  
→  238      def report_error(self, e):
   239          original = getattr(e, "original_exception", None)
   240  
   241          if original is not None:
```

### row #80 — `tests.test_user_error_handler.test_error_handler_http_subclass` @ `tests/test_user_error_handler.py:94`

```python
    91      assert c.get("/child-registered").data == b"child-registered"
    92  
    93  
→   94  def test_error_handler_http_subclass(app):
    95      class ForbiddenSubclassRegistered(Forbidden):
    96          pass
    97  
```

### row #81 — `tests.test_user_error_handler.test_error_handler_no_match` @ `tests/test_user_error_handler.py:10`

```python
     7  import flask
     8  
     9  
→   10  def test_error_handler_no_match(app, client):
    11      class CustomException(Exception):
    12          pass
    13  
```

### row #82 — `tests.test_views.test_implicit_head` @ `tests/test_views.py:141`

```python
   138      assert "OPTIONS" in rv.allow
   139  
   140  
→  141  def test_implicit_head(app, client):
   142      class Index(flask.views.MethodView):
   143          def get(self):
   144              return flask.Response("Blub", headers={"X-Method": flask.request.method})
```

## PublicSymbol (37 rows)

### row #83 — `docs.conf.issues_github_path` @ `docs/conf.py:34`

```python
    31      "wtforms": ("https://wtforms.readthedocs.io/", None),
    32      "blinker": ("https://pythonhosted.org/blinker/", None),
    33  }
→   34  issues_github_path = "pallets/flask"
    35  
    36  # HTML -----------------------------------------------------------------
    37  
```

### row #84 — `examples.tutorial.flaskr.db.init_app` @ `examples/tutorial/flaskr/db.py:49`

```python
    46      click.echo("Initialized the database.")
    47  
    48  
→   49  def init_app(app):
    50      """Register database functions with the Flask app. This is called by
    51      the application factory.
    52      """
```

### row #85 — `examples.tutorial.tests.test_db.test_get_close_db` @ `examples/tutorial/tests/test_db.py:8`

```python
     5  from flaskr.db import get_db
     6  
     7  
→    8  def test_get_close_db(app):
     9      with app.app_context():
    10          db = get_db()
    11          assert db is get_db()
```

### row #86 — `src.flask.cli.main` @ `src/flask/cli.py:981`

```python
   978  )
   979  
   980  
→  981  def main() -> None:
   982      if int(click.__version__[0]) < 8:
   983          warnings.warn(
   984              "Using the `flask` cli with Click 7 is deprecated and"
```

### row #87 — `src.flask.config.ConfigAttribute` @ `src/flask/config.py:9`

```python
     6  from werkzeug.utils import import_string
     7  
     8  
→    9  class ConfigAttribute:
    10      """Makes an attribute forward to the config"""
    11  
    12      def __init__(self, name: str, get_converter: t.Optional[t.Callable] = None) -> None:
```

### row #88 — `src.flask.helpers.get_load_dotenv` @ `src/flask/helpers.py:49`

```python
    46      return val.lower() not in ("0", "false", "no")
    47  
    48  
→   49  def get_load_dotenv(default: bool = True) -> bool:
    50      """Get whether the user has disabled loading dotenv files by setting
    51      :envvar:`FLASK_SKIP_DOTENV`. The default is ``True``, load the
    52      files.
```

### row #89 — `src.flask.json.__init__.JSONDecoder` @ `src/flask/json/__init__.py:59`

```python
    56          return super().default(o)
    57  
    58  
→   59  class JSONDecoder(_json.JSONDecoder):
    60      """The default JSON decoder.
    61  
    62      This does not change any behavior from the built-in
```

### row #90 — `src.flask.json.tag.TagTuple` @ `src/flask/json/tag.py:130`

```python
   127      tag = to_json
   128  
   129  
→  130  class TagTuple(JSONTag):
   131      __slots__ = ()
   132      key = " t"
   133  
```

### row #91 — `src.flask.json.tag.TaggedJSONSerializer` @ `src/flask/json/tag.py:216`

```python
   213          return parse_date(value)
   214  
   215  
→  216  class TaggedJSONSerializer:
   217      """Serializer that uses a tag system to compactly represent objects that
   218      are not JSON types. Passed as the intermediate serializer to
   219      :class:`itsdangerous.Serializer`.
```

### row #92 — `src.flask.templating.Environment` @ `src/flask/templating.py:33`

```python
    30      return rv
    31  
    32  
→   33  class Environment(BaseEnvironment):
    34      """Works like a regular Jinja2 environment but has some additional
    35      knowledge of how Flask's blueprint works so that it can prepend the
    36      name of the blueprint to referenced templates if necessary.
```

### row #93 — `src.flask.typing.ErrorHandlerCallable` @ `src/flask/typing.py:39`

```python
    36  AppOrBlueprintKey = t.Optional[str]  # The App key is None, whereas blueprints are named
    37  AfterRequestCallable = t.Callable[["Response"], "Response"]
    38  BeforeRequestCallable = t.Callable[[], None]
→   39  ErrorHandlerCallable = t.Callable[[Exception], ResponseReturnValue]
    40  TeardownCallable = t.Callable[[t.Optional[BaseException]], "Response"]
    41  TemplateContextProcessorCallable = t.Callable[[], t.Dict[str, t.Any]]
    42  TemplateFilterCallable = t.Callable[[t.Any], str]
```

### row #94 — `src.flask.typing.HeaderValue` @ `src/flask/typing.py:20`

```python
    17  
    18  # the possible types for an individual HTTP header
    19  HeaderName = str
→   20  HeaderValue = t.Union[str, t.List[str], t.Tuple[str, ...]]
    21  
    22  # the possible types for HTTP headers
    23  HeadersValue = t.Union[
```

### row #95 — `tests.test_apps.blueprintapp.__init__.app` @ `tests/test_apps/blueprintapp/__init__.py:3`

```python
     1  from flask import Flask
     2  
→    3  app = Flask(__name__)
     4  app.config["DEBUG"] = True
     5  from blueprintapp.apps.admin import admin
     6  from blueprintapp.apps.frontend import frontend
```

### row #96 — `tests.test_apps.helloworld.hello.app` @ `tests/test_apps/helloworld/hello.py:3`

```python
     1  from flask import Flask
     2  
→    3  app = Flask(__name__)
     4  
     5  
     6  @app.route("/")
```

### row #97 — `tests.test_basic.test_extended_flashing` @ `tests/test_basic.py:606`

```python
   603      assert list(flask.get_flashed_messages()) == ["Zap", "Zip"]
   604  
   605  
→  606  def test_extended_flashing(app):
   607      # Be sure app.testing=True below, else tests can fail silently.
   608      #
   609      # Specifically, if app.testing is not set to True, the AssertionErrors
```

### row #98 — `tests.test_basic.test_http_error_subclass_handling` @ `tests/test_basic.py:977`

```python
   974      assert client.get("/").data == b"42"
   975  
   976  
→  977  def test_http_error_subclass_handling(app, client):
   978      class ForbiddenSubclass(Forbidden):
   979          pass
   980  
```

### row #99 — `tests.test_basic.test_inject_blueprint_url_defaults` @ `tests/test_basic.py:1633`

```python
  1630      assert client.get("/foo").data == b"/en/about"
  1631  
  1632  
→ 1633  def test_inject_blueprint_url_defaults(app):
  1634      bp = flask.Blueprint("foo.bar.baz", __name__, template_folder="template")
  1635  
  1636      @bp.url_defaults
```

### row #100 — `tests.test_basic.test_jsonify_prettyprint` @ `tests/test_basic.py:1308`

```python
  1305      assert rv.data == compressed_msg
  1306  
  1307  
→ 1308  def test_jsonify_prettyprint(app, req_ctx):
  1309      app.config.update({"JSONIFY_PRETTYPRINT_REGULAR": True})
  1310      compressed_msg = {"msg": {"submsg": "W00t"}, "msg2": "foobar"}
  1311      pretty_response = (
```

### row #101 — `tests.test_basic.test_session_stored_last` @ `tests/test_basic.py:456`

```python
   453      assert match is None
   454  
   455  
→  456  def test_session_stored_last(app, client):
   457      @app.after_request
   458      def modify_session(response):
   459          flask.session["foo"] = 42
```

### row #102 — `tests.test_basic.test_subdomain_basic_support` @ `tests/test_basic.py:1842`

```python
  1839      assert sorted(flask.g) == ["bar", "foo"]
  1840  
  1841  
→ 1842  def test_subdomain_basic_support():
  1843      app = flask.Flask(__name__, subdomain_matching=True)
  1844      app.config["SERVER_NAME"] = "localhost.localdomain"
  1845      client = app.test_client()
```

### row #103 — `tests.test_basic.test_teardown_request_handler_error` @ `tests/test_basic.py:790`

```python
   787      assert len(called) == 1
   788  
   789  
→  790  def test_teardown_request_handler_error(app, client):
   791      called = []
   792      app.testing = False
   793  
```

### row #104 — `tests.test_basic.test_trapping_of_bad_request_key_errors` @ `tests/test_basic.py:1042`

```python
  1039      assert rv.data == b"E2"
  1040  
  1041  
→ 1042  def test_trapping_of_bad_request_key_errors(app, client):
  1043      @app.route("/key")
  1044      def fail():
  1045          flask.request.form["missing_key"]
```

### row #105 — `tests.test_blueprints.test_blueprint_url_processors` @ `tests/test_blueprints.py:153`

```python
   150      assert client.get("/2/bar").data == b"19"
   151  
   152  
→  153  def test_blueprint_url_processors(app, client):
   154      bp = flask.Blueprint("frontend", __name__, url_prefix="/<lang_code>")
   155  
   156      @bp.url_defaults
```

### row #106 — `tests.test_blueprints.test_route_decorator_custom_endpoint` @ `tests/test_blueprints.py:311`

```python
   308      assert client.get("/page/2").data == b"2"
   309  
   310  
→  311  def test_route_decorator_custom_endpoint(app, client):
   312      bp = flask.Blueprint("bp", __name__)
   313  
   314      @bp.route("/foo")
```

### row #107 — `tests.test_blueprints.test_templates_list` @ `tests/test_blueprints.py:249`

```python
   246          app.config["SEND_FILE_MAX_AGE_DEFAULT"] = max_age_default
   247  
   248  
→  249  def test_templates_list(test_apps):
   250      from blueprintapp import app
   251  
   252      templates = sorted(app.jinja_env.list_templates())
```

### row #108 — `tests.test_cli.test_get_version` @ `tests/test_cli.py:254`

```python
   251          locate_app(info, "cliapp.importerrorapp", None, raise_if_not_found=False)
   252  
   253  
→  254  def test_get_version(test_apps, capsys):
   255      from flask import __version__ as flask_version
   256      from werkzeug import __version__ as werkzeug_version
   257      from platform import python_version
```

### row #109 — `tests.test_cli.test_path` @ `tests/test_cli.py:33`

```python
    30  from flask.cli import with_appcontext
    31  
    32  cwd = os.getcwd()
→   33  test_path = os.path.abspath(os.path.join(os.path.dirname(__file__), "test_apps"))
    34  
    35  
    36  @pytest.fixture
```

### row #110 — `tests.test_cli.test_run_cert_adhoc` @ `tests/test_cli.py:570`

```python
   567      assert ctx.params["cert"] == (__file__, __file__)
   568  
   569  
→  570  def test_run_cert_adhoc(monkeypatch):
   571      monkeypatch.setitem(sys.modules, "cryptography", None)
   572  
   573      # cryptography not installed
```

### row #111 — `tests.test_config.test_config_from_mapping` @ `tests/test_config.py:41`

```python
    38      common_object_test(app)
    39  
    40  
→   41  def test_config_from_mapping():
    42      app = flask.Flask(__name__)
    43      app.config.from_mapping({"SECRET_KEY": "config", "TEST_KEY": "foo"})
    44      common_object_test(app)
```

### row #112 — `tests.test_config.test_session_lifetime` @ `tests/test_config.py:136`

```python
   133      common_object_test(app)
   134  
   135  
→  136  def test_session_lifetime():
   137      app = flask.Flask(__name__)
   138      app.config["PERMANENT_SESSION_LIFETIME"] = 42
   139      assert app.permanent_session_lifetime.seconds == 42
```

### row #113 — `tests.test_converters.test_custom_converters` @ `tests/test_converters.py:7`

```python
     4  from flask import url_for
     5  
     6  
→    7  def test_custom_converters(app, client):
     8      class ListConverter(BaseConverter):
     9          def to_python(self, value):
    10              return value.split(",")
```

### row #114 — `tests.test_instance_config.test_installed_package_paths` @ `tests/test_instance_config.py:75`

```python
    72      assert app.instance_path == modules_tmpdir.join("var").join("site_app-instance")
    73  
    74  
→   75  def test_installed_package_paths(
    76      limit_loader, modules_tmpdir, modules_tmpdir_prefix, purge_module, monkeypatch
    77  ):
    78      installed_path = modules_tmpdir.mkdir("path")
```

### row #115 — `tests.test_json_tag.test_duplicate_tag` @ `tests/test_json_tag.py:32`

```python
    29      assert s.loads(s.dumps(data)) == data
    30  
    31  
→   32  def test_duplicate_tag():
    33      class TagDict(JSONTag):
    34          key = " d"
    35  
```

### row #116 — `tests.test_templating.test_template_test_with_name` @ `tests/test_templating.py:227`

```python
   224      assert app.jinja_env.tests["boolean"](False)
   225  
   226  
→  227  def test_template_test_with_name(app):
   228      @app.template_test("boolean")
   229      def is_boolean(value):
   230          return isinstance(value, bool)
```

### row #117 — `tests.test_testing.test_blueprint_with_subdomain` @ `tests/test_testing.py:119`

```python
   116      assert eb.input_stream.read().decode("utf8") == '"\u20ac"'
   117  
   118  
→  119  def test_blueprint_with_subdomain():
   120      app = flask.Flask(__name__, subdomain_matching=True)
   121      app.config["SERVER_NAME"] = "example.com:1234"
   122      app.config["APPLICATION_ROOT"] = "/foo"
```

### row #118 — `tests.test_testing.test_cli_custom_obj` @ `tests/test_testing.py:394`

```python
   391      assert "Hello" in result.output
   392  
   393  
→  394  def test_cli_custom_obj(app):
   395      class NS:
   396          called = False
   397  
```

### row #119 — `tests.test_testing.test_client_pop_all_preserved` @ `tests/test_testing.py:412`

```python
   409      assert NS.called
   410  
   411  
→  412  def test_client_pop_all_preserved(app, req_ctx, client):
   413      @app.route("/")
   414      def index():
   415          # stream_with_context pushes a third context, preserved by client
```

## TestAssertion (81 rows)

### row #120 — `TestRoutes.test_no_routes` @ `tests/test_cli.py:501`

```python
   498      def test_no_routes(self, invoke_no_routes):
   499          result = invoke_no_routes(["routes"])
   500          assert result.exit_code == 0
→  501          assert "No routes were registered." in result.output
   502  
   503  
   504  need_dotenv = pytest.mark.skipif(dotenv is None, reason="dotenv is not installed")
```

### row #121 — `TestSendfile.test_static_file` @ `tests/test_helpers.py:55`

```python
    52  
    53          # Test with direct use of send_file.
    54          rv = flask.send_file("static/index.html")
→   55          assert rv.cache_control.max_age is None
    56          rv.close()
    57  
    58          app.config["SEND_FILE_MAX_AGE_DEFAULT"] = 3600
```

### row #122 — `TestSendfile.test_static_file` @ `tests/test_helpers.py:84`

```python
    81          with app.test_request_context():
    82              # Test with static file handler.
    83              rv = app.send_static_file("index.html")
→   84              assert rv.cache_control.max_age == 10
    85              rv.close()
    86  
    87              # Test with direct use of send_file.
```

### row #123 — `TestUrlFor.test_url_for_with_alternating_schemes` @ `tests/test_helpers.py:132`

```python
   129          def index():
   130              return "42"
   131  
→  132          assert flask.url_for("index", _external=True) == "http://localhost/"
   133          assert (
   134              flask.url_for("index", _external=True, _scheme="https")
   135              == "https://localhost/"
```

### row #124 — `TestUrlFor.test_url_with_method` @ `tests/test_helpers.py:156`

```python
   153          app.add_url_rule("/myview/<int:id>", methods=["GET"], view_func=myview)
   154          app.add_url_rule("/myview/create", methods=["POST"], view_func=myview)
   155  
→  156          assert flask.url_for("myview", _method="GET") == "/myview/"
   157          assert flask.url_for("myview", id=42, _method="GET") == "/myview/42"
   158          assert flask.url_for("myview", _method="POST") == "/myview/create"
   159  
```

### row #125 — `test_add_template_filter_with_name` @ `tests/test_blueprints.py:469`

```python
   466      bp.add_app_template_filter(my_reverse, "strrev")
   467      app.register_blueprint(bp, url_prefix="/py")
   468      assert "strrev" in app.jinja_env.filters.keys()
→  469      assert app.jinja_env.filters["strrev"] == my_reverse
   470      assert app.jinja_env.filters["strrev"]("abcd") == "dcba"
   471  
   472  
```

### row #126 — `test_add_template_filter_with_name_and_template` @ `tests/test_blueprints.py:554`

```python
   551          return flask.render_template("template_filter.html", value="abcd")
   552  
   553      rv = client.get("/")
→  554      assert rv.data == b"dcba"
   555  
   556  
   557  def test_template_test(app):
```

### row #127 — `test_add_template_test_with_name_and_template` @ `tests/test_blueprints.py:690`

```python
   687          return flask.render_template("template_test.html", value=False)
   688  
   689      rv = client.get("/")
→  690      assert b"Success!" in rv.data
   691  
   692  
   693  def test_context_processing(app, client):
```

### row #128 — `test_app_ctx_globals_methods` @ `tests/test_appctx.py:152`

```python
   149      # __iter__
   150      assert list(flask.g) == ["foo"]
   151      # __repr__
→  152      assert repr(flask.g) == "<flask.g of 'flask_test'>"
   153  
   154  
   155  def test_custom_app_ctx_globals_class(app):
```

### row #129 — `test_author_required` @ `examples/tutorial/tests/test_blog.py:37`

```python
    34      assert client.post("/1/update").status_code == 403
    35      assert client.post("/1/delete").status_code == 403
    36      # current user doesn't see edit link
→   37      assert b'href="/1/update"' not in client.get("/").data
    38  
    39  
    40  @pytest.mark.parametrize("path", ("/2/update", "/2/delete"))
```

### row #130 — `test_before_first_request_functions_concurrent` @ `tests/test_basic.py:1723`

```python
  1720      t.start()
  1721      get_and_assert()
  1722      t.join()
→ 1723      assert app.got_first_request
  1724  
  1725  
  1726  def test_routing_redirect_debugging(app, client):
```

### row #131 — `test_before_request_and_routing_errors` @ `tests/test_basic.py:957`

```python
   954          return flask.g.something, 404
   955  
   956      rv = client.get("/")
→  957      assert rv.status_code == 404
   958      assert rv.data == b"value"
   959  
   960  
```

### row #132 — `test_blueprint_app_error_handling` @ `tests/test_blueprints.py:103`

```python
   100      app.register_blueprint(forbidden_bp)
   101  
   102      assert client.get("/forbidden").data == b"you shall not pass"
→  103      assert client.get("/nope").data == b"you shall not pass"
   104  
   105  
   106  @pytest.mark.parametrize(
```

### row #133 — `test_blueprint_specific_error_handling` @ `tests/test_blueprints.py:45`

```python
    42  
    43      assert client.get("/frontend-no").data == b"frontend says no"
    44      assert client.get("/backend-no").data == b"backend says no"
→   45      assert client.get("/what-is-a-sideend").data == b"application itself says no"
    46  
    47  
    48  def test_blueprint_specific_user_error_handling(app, client):
```

### row #134 — `test_blueprint_specific_user_error_handling` @ `tests/test_blueprints.py:59`

```python
    56  
    57      @blue.errorhandler(MyDecoratorException)
    58      def my_decorator_exception_handler(e):
→   59          assert isinstance(e, MyDecoratorException)
    60          return "boom"
    61  
    62      def my_function_exception_handler(e):
```

### row #135 — `test_blueprint_url_defaults` @ `tests/test_blueprints.py:148`

```python
   145      app.register_blueprint(bp, url_prefix="/2", url_defaults={"bar": 19})
   146  
   147      assert client.get("/1/foo").data == b"23/42"
→  148      assert client.get("/2/foo").data == b"19/42"
   149      assert client.get("/1/bar").data == b"23"
   150      assert client.get("/2/bar").data == b"19"
   151  
```

### row #136 — `test_blueprint_url_processors` @ `tests/test_blueprints.py:174`

```python
   171  
   172      app.register_blueprint(bp)
   173  
→  174      assert client.get("/de/").data == b"/de/about"
   175      assert client.get("/de/about").data == b"/de/"
   176  
   177  
```

### row #137 — `test_cli_runner_class` @ `tests/test_testing.py:377`

```python
   374  
   375      app.test_cli_runner_class = SubRunner
   376      runner = app.test_cli_runner()
→  377      assert isinstance(runner, SubRunner)
   378  
   379  
   380  def test_cli_invoke(app):
```

### row #138 — `test_client_open_environ` @ `tests/test_testing.py:84`

```python
    81      request.addfinalizer(builder.close)
    82  
    83      rv = client.open(builder)
→   84      assert rv.data == b"127.0.0.1"
    85  
    86      environ = builder.get_environ()
    87      client.environ_base["REMOTE_ADDR"] = "127.0.0.2"
```

### row #139 — `test_client_pop_all_preserved` @ `tests/test_testing.py:424`

```python
   421          client.get("/")
   422  
   423      # only req_ctx fixture should still be pushed
→  424      assert flask._request_ctx_stack.top is req_ctx
```

### row #140 — `test_config` @ `examples/tutorial/tests/test_factory.py:7`

```python
     4  def test_config():
     5      """Test create_app without passing test config."""
     6      assert not create_app().testing
→    7      assert create_app({"TESTING": True}).testing
     8  
     9  
    10  def test_hello(client):
```

### row #141 — `test_config_from_envvar` @ `tests/test_config.py:76`

```python
    73      app = flask.Flask(__name__)
    74      with pytest.raises(RuntimeError) as e:
    75          app.config.from_envvar("FOO_SETTINGS")
→   76          assert "'FOO_SETTINGS' is not set" in str(e.value)
    77      assert not app.config.from_envvar("FOO_SETTINGS", silent=True)
    78  
    79      monkeypatch.setattr(
```

### row #142 — `test_custom_config_class` @ `tests/test_config.py:131`

```python
   128          config_class = Config
   129  
   130      app = Flask(__name__)
→  131      assert isinstance(app.config, Config)
   132      app.config.from_object(__name__)
   133      common_object_test(app)
   134  
```

### row #143 — `test_custom_jinja_env` @ `tests/test_templating.py:443`

```python
   440          jinja_environment = CustomEnvironment
   441  
   442      app = CustomFlask(__name__)
→  443      assert isinstance(app.jinja_env, CustomEnvironment)
```

### row #144 — `test_default_error_handler` @ `tests/test_user_error_handler.py:183`

```python
   180  
   181      @app.errorhandler(HTTPException)
   182      def catchall_exception_handler(e):
→  183          assert isinstance(e, HTTPException)
   184          assert isinstance(e, NotFound)
   185          return "default"
   186  
```

### row #145 — `test_environ_base_default` @ `tests/test_testing.py:52`

```python
    49          return flask.request.remote_addr
    50  
    51      rv = client.get("/")
→   52      assert rv.data == b"127.0.0.1"
    53      assert flask.g.user_agent == f"werkzeug/{werkzeug.__version__}"
    54  
    55  
```

### row #146 — `test_environ_defaults` @ `tests/test_testing.py:42`

```python
    39      assert ctx.request.url == "http://localhost/"
    40      with client:
    41          rv = client.get("/")
→   42          assert rv.data == b"http://localhost/"
    43  
    44  
    45  def test_environ_base_default(app, client, app_ctx):
```

### row #147 — `test_environ_defaults_from_config` @ `tests/test_testing.py:27`

```python
    24          return flask.request.url
    25  
    26      ctx = app.test_request_context()
→   27      assert ctx.request.url == "http://example.com:1234/foo/"
    28  
    29      rv = client.get("/")
    30      assert rv.data == b"http://example.com:1234/foo/"
```

### row #148 — `test_error_handler_http_subclass` @ `tests/test_user_error_handler.py:108`

```python
   105  
   106      @app.errorhandler(ForbiddenSubclassRegistered)
   107      def subclass_exception_handler(e):
→  108          assert isinstance(e, ForbiddenSubclassRegistered)
   109          return "forbidden-registered"
   110  
   111      @app.route("/forbidden")
```

### row #149 — `test_error_handler_subclass` @ `tests/test_user_error_handler.py:72`

```python
    69  
    70      @app.errorhandler(ChildExceptionRegistered)
    71      def child_exception_handler(e):
→   72          assert isinstance(e, ChildExceptionRegistered)
    73          return "child-registered"
    74  
    75      @app.route("/parent")
```

### row #150 — `test_extended_flashing` @ `tests/test_basic.py:654`

```python
   651          messages = flask.get_flashed_messages(
   652              category_filter=["message", "warning"], with_categories=True
   653          )
→  654          assert list(messages) == [
   655              ("message", "Hello World"),
   656              ("warning", flask.Markup("<em>Testing</em>")),
   657          ]
```

### row #151 — `test_flashes` @ `tests/test_basic.py:598`

```python
   595  
   596  
   597  def test_flashes(app, req_ctx):
→  598      assert not flask.session.modified
   599      flask.flash("Zap")
   600      flask.session.modified = False
   601      flask.flash("Zip")
```

### row #152 — `test_full_url_request` @ `tests/test_testing.py:287`

```python
   284  
   285      with client:
   286          rv = client.post("http://domain.com/action?vodka=42", data={"gin": 43})
→  287          assert rv.status_code == 200
   288          assert "gin" in flask.request.form
   289          assert "vodka" in flask.request.args
   290  
```

### row #153 — `test_get_method_on_g` @ `tests/test_basic.py:1828`

```python
  1825  
  1826  def test_get_method_on_g(app_ctx):
  1827      assert flask.g.get("x") is None
→ 1828      assert flask.g.get("x", 11) == 11
  1829      flask.g.x = 42
  1830      assert flask.g.get("x") == 42
  1831      assert flask.g.x == 42
```

### row #154 — `test_get_namespace` @ `tests/test_config.py:165`

```python
   162      assert "bar stuff 1" == bar_options["STUFF_1"]
   163      assert "bar stuff 2" == bar_options["STUFF_2"]
   164      foo_options = app.config.get_namespace("FOO_", trim_namespace=False)
→  165      assert 2 == len(foo_options)
   166      assert "foo option 1" == foo_options["foo_option_1"]
   167      assert "foo option 2" == foo_options["foo_option_2"]
   168      bar_options = app.config.get_namespace(
```

### row #155 — `test_get_namespace` @ `tests/test_config.py:172`

```python
   169          "BAR_", lowercase=False, trim_namespace=False
   170      )
   171      assert 2 == len(bar_options)
→  172      assert "bar stuff 1" == bar_options["BAR_STUFF_1"]
   173      assert "bar stuff 2" == bar_options["BAR_STUFF_2"]
   174  
   175  
```

### row #156 — `test_get_version` @ `tests/test_cli.py:271`

```python
   268      out, err = capsys.readouterr()
   269      assert f"Python {python_version()}" in out
   270      assert f"Flask {flask_version}" in out
→  271      assert f"Werkzeug {werkzeug_version}" in out
   272  
   273  
   274  def test_scriptinfo(test_apps, monkeypatch):
```

### row #157 — `test_help_echo_exception` @ `tests/test_cli.py:430`

```python
   427      result = runner.invoke(cli, ["--help"])
   428      assert result.exit_code == 0
   429      assert "Exception: oh no" in result.stderr
→  430      assert "Usage:" in result.stdout
   431  
   432  
   433  class TestRoutes:
```

### row #158 — `test_index` @ `examples/tutorial/tests/test_blog.py:15`

```python
    12      response = client.get("/")
    13      assert b"test title" in response.data
    14      assert b"by test on 2018-01-01" in response.data
→   15      assert b"test\nbody" in response.data
    16      assert b'href="/1/update"' in response.data
    17  
    18  
```

### row #159 — `test_inject_blueprint_url_defaults` @ `tests/test_basic.py:1649`

```python
  1646      values = dict()
  1647      app.inject_url_defaults("foo.bar.baz.view", values)
  1648      expected = dict(page="login")
→ 1649      assert values == expected
  1650  
  1651      with app.test_request_context("/somepage"):
  1652          url = flask.url_for("foo.bar.baz.view")
```

### row #160 — `test_json_request_and_response` @ `tests/test_testing.py:307`

```python
   304  
   305          # Response should be in JSON
   306          assert rv.status_code == 200
→  307          assert rv.is_json
   308          assert rv.get_json() == json_data
   309  
   310  
```

### row #161 — `test_jsonify_dicts` @ `tests/test_json.py:100`

```python
    97  
    98      for url in "/kw", "/dict":
    99          rv = client.get(url)
→  100          assert rv.mimetype == "application/json"
   101          assert flask.json.loads(rv.data) == d
   102  
   103  
```

### row #162 — `test_jsonify_mimetype` @ `tests/test_basic.py:1323`

```python
  1320      app.config.update({"JSONIFY_MIMETYPE": "application/vnd.api+json"})
  1321      msg = {"msg": {"submsg": "W00t"}}
  1322      rv = flask.make_response(flask.jsonify(msg), 200)
→ 1323      assert rv.mimetype == "application/vnd.api+json"
  1324  
  1325  
  1326  @pytest.mark.skipif(sys.version_info < (3, 7), reason="requires Python >= 3.7")
```

### row #163 — `test_jsonify_no_prettyprint` @ `tests/test_basic.py:1305`

```python
  1302      uncompressed_msg = {"msg": {"submsg": "W00t"}, "msg2": "foobar"}
  1303  
  1304      rv = flask.make_response(flask.jsonify(uncompressed_msg), 200)
→ 1305      assert rv.data == compressed_msg
  1306  
  1307  
  1308  def test_jsonify_prettyprint(app, req_ctx):
```

### row #164 — `test_locate_app_suppress_raise` @ `tests/test_cli.py:247`

```python
   244  def test_locate_app_suppress_raise(test_apps):
   245      info = ScriptInfo()
   246      app = locate_app(info, "notanapp.py", None, raise_if_not_found=False)
→  247      assert app is None
   248  
   249      # only direct import error is suppressed
   250      with pytest.raises(NoAppException):
```

### row #165 — `test_log_view_exception` @ `tests/test_logging.py:94`

```python
    91      app.testing = False
    92      stream = StringIO()
    93      rv = client.get("/", errors_stream=stream)
→   94      assert rv.status_code == 500
    95      assert rv.data
    96      err = stream.getvalue()
    97      assert "Exception on / [GET]" in err
```

### row #166 — `test_log_view_exception` @ `tests/test_logging.py:95`

```python
    92      stream = StringIO()
    93      rv = client.get("/", errors_stream=stream)
    94      assert rv.status_code == 500
→   95      assert rv.data
    96      err = stream.getvalue()
    97      assert "Exception on / [GET]" in err
    98      assert "Exception: test" in err
```

### row #167 — `test_logger` @ `tests/test_logging.py:37`

```python
    34  
    35  
    36  def test_logger(app):
→   37      assert app.logger.name == "flask_test"
    38      assert app.logger.level == logging.NOTSET
    39      assert app.logger.handlers == [default_handler]
    40  
```

### row #168 — `test_logger` @ `tests/test_logging.py:39`

```python
    36  def test_logger(app):
    37      assert app.logger.name == "flask_test"
    38      assert app.logger.level == logging.NOTSET
→   39      assert app.logger.handlers == [default_handler]
    40  
    41  
    42  def test_logger_debug(app):
```

### row #169 — `test_make_response_with_response_instance` @ `tests/test_basic.py:1296`

```python
  1293      )
  1294      assert rv.status_code == 400
  1295      assert rv.headers["Content-Type"] == "text/html"
→ 1296      assert rv.headers["X-Foo"] == "bar"
  1297  
  1298  
  1299  def test_jsonify_no_prettyprint(app, req_ctx):
```

### row #170 — `test_max_cookie_size` @ `tests/test_basic.py:2006`

```python
  2003          return r
  2004  
  2005      client.get("/")
→ 2006      assert len(recwarn) == 1
  2007      w = recwarn.pop()
  2008      assert "cookie is too large" in str(w.message)
  2009  
```

### row #171 — `test_methods_var_inheritance` @ `tests/test_views.py:204`

```python
   201  
   202      assert client.get("/").data == b"GET"
   203      assert client.open("/", method="PROPFIND").data == b"PROPFIND"
→  204      assert ChildView.methods == {"PROPFIND", "GET"}
   205  
   206  
   207  def test_multiple_inheritance(app, client):
```

### row #172 — `test_multiple_inheritance` @ `tests/test_views.py:221`

```python
   218  
   219      app.add_url_rule("/", view_func=GetDeleteView.as_view("index"))
   220  
→  221      assert client.get("/").data == b"GET"
   222      assert client.delete("/").data == b"DELETE"
   223      assert sorted(GetDeleteView.methods) == ["DELETE", "GET"]
   224  
```

### row #173 — `test_nested_blueprint` @ `tests/test_blueprints.py:897`

```python
   894      app.register_blueprint(parent, url_prefix="/parent")
   895  
   896      assert client.get("/parent/").data == b"Parent yes"
→  897      assert client.get("/parent/child/").data == b"Child yes"
   898      assert client.get("/parent/child/grandchild/").data == b"Grandchild yes"
   899      assert client.get("/parent/no").data == b"Parent no"
   900      assert client.get("/parent/child/no").data == b"Parent no"
```

### row #174 — `test_no_command_echo_loading_error` @ `tests/test_cli.py:406`

```python
   403  
   404      runner = CliRunner(mix_stderr=False)
   405      result = runner.invoke(cli, ["missing"])
→  406      assert result.exit_code == 2
   407      assert "FLASK_APP" in result.stderr
   408      assert "Usage:" in result.stderr
   409  
```

### row #175 — `test_nosubdomain` @ `tests/test_testing.py:364`

```python
   361      with client:
   362          response = client.get(url)
   363  
→  364      assert 200 == response.status_code
   365      assert b"xxx" == response.data
   366  
   367  
```

### row #176 — `test_request_dispatching` @ `tests/test_basic.py:142`

```python
   139      assert sorted(rv.allow) == ["GET", "HEAD", "OPTIONS"]
   140      rv = client.head("/")
   141      assert rv.status_code == 200
→  142      assert not rv.data  # head truncates
   143      assert client.post("/more").data == b"POST"
   144      assert client.get("/more").data == b"GET"
   145      rv = client.delete("/more")
```

### row #177 — `test_response_type_errors` @ `tests/test_basic.py:1246`

```python
  1243      with pytest.raises(TypeError) as e:
  1244          c.get("/none")
  1245          assert "returned None" in str(e.value)
→ 1246          assert "from_none" in str(e.value)
  1247  
  1248      with pytest.raises(TypeError) as e:
  1249          c.get("/small_tuple")
```

### row #178 — `test_response_types` @ `tests/test_basic.py:1189`

```python
  1186      rv = client.get("/text_headers")
  1187      assert rv.data == b"Hello"
  1188      assert rv.headers["X-Foo"] == "Test"
→ 1189      assert rv.status_code == 200
  1190      assert rv.mimetype == "text/plain"
  1191  
  1192      rv = client.get("/text_status")
```

### row #179 — `test_response_types` @ `tests/test_basic.py:1201`

```python
  1198      assert rv.data == b"Hello world"
  1199      assert rv.content_type == "text/plain"
  1200      assert rv.headers.getlist("X-Foo") == ["Bar"]
→ 1201      assert rv.headers["X-Bar"] == "Foo"
  1202      assert rv.status_code == 404
  1203  
  1204      rv = client.get("/response_status")
```

### row #180 — `test_response_types` @ `tests/test_basic.py:1202`

```python
  1199      assert rv.content_type == "text/plain"
  1200      assert rv.headers.getlist("X-Foo") == ["Bar"]
  1201      assert rv.headers["X-Bar"] == "Foo"
→ 1202      assert rv.status_code == 404
  1203  
  1204      rv = client.get("/response_status")
  1205      assert rv.data == b"Hello world"
```

### row #181 — `test_route_decorator_custom_endpoint_with_dots` @ `tests/test_blueprints.py:401`

```python
   398      rv = client.get("/py/bar")
   399      assert rv.status_code == 404
   400      rv = client.get("/py/bar/123")
→  401      assert rv.status_code == 404
   402  
   403  
   404  def test_endpoint_decorator(app, client):
```

### row #182 — `test_run_cert_import` @ `tests/test_cli.py:602`

```python
   599  
   600      monkeypatch.setitem(sys.modules, "ssl_context", ssl_context)
   601      ctx = run_command.make_context("run", ["--cert", "ssl_context"])
→  602      assert ctx.params["cert"] is ssl_context
   603  
   604      # no --key with SSLContext
   605      with pytest.raises(click.BadParameter):
```

### row #183 — `test_scriptinfo` @ `tests/test_cli.py:291`

```python
   288      obj = ScriptInfo(app_import_path=f"{cli_app_path}:testapp")
   289      app = obj.load_app()
   290      assert app.name == "testapp"
→  291      assert obj.load_app() is app
   292  
   293      def create_app():
   294          return Flask("createapp")
```

### row #184 — `test_session_error_pops_context` @ `tests/test_reqctx.py:218`

```python
   215          AssertionError()
   216  
   217      response = app.test_client().get("/")
→  218      assert response.status_code == 500
   219      assert not flask.request
   220      assert not flask.current_app
   221  
```

### row #185 — `test_session_special_types` @ `tests/test_basic.py:494`

```python
   491          assert s["b"] == b"\xff"
   492          assert type(s["m"]) == flask.Markup
   493          assert s["m"] == flask.Markup("<html>")
→  494          assert s["u"] == the_uuid
   495          assert s["d"] == now
   496          assert s["t_tag"] == {" t": "not-a-tuple"}
   497          assert s["di_t_tag"] == {" t__": "not-a-tuple"}
```

### row #186 — `test_session_using_application_root` @ `tests/test_basic.py:315`

```python
   312          return "Hello World"
   313  
   314      rv = client.get("/", "http://example.com:8080/")
→  315      assert "path=/bar" in rv.headers["set-cookie"].lower()
   316  
   317  
   318  def test_session_using_session_settings(app, client):
```

### row #187 — `test_session_using_session_settings` @ `tests/test_basic.py:339`

```python
   336      assert "domain=.example.com" in cookie
   337      assert "path=/" in cookie
   338      assert "secure" in cookie
→  339      assert "httponly" not in cookie
   340      assert "samesite" in cookie
   341  
   342      @app.route("/clear")
```

### row #188 — `test_static_route_with_host_matching` @ `tests/test_basic.py:1476`

```python
  1473      app = flask.Flask(__name__, host_matching=True, static_host="example.com")
  1474      c = app.test_client()
  1475      rv = c.get("http://example.com/static/index.html")
→ 1476      assert rv.status_code == 200
  1477      rv.close()
  1478      with app.test_request_context():
  1479          rv = flask.url_for("static", filename="index.html", _external=True)
```

### row #189 — `test_subdomain_matching` @ `tests/test_basic.py:1872`

```python
  1869          return f"index for {user}"
  1870  
  1871      rv = client.get("/", "http://mitsuhiko.localhost.localdomain/")
→ 1872      assert rv.data == b"index for mitsuhiko"
  1873  
  1874  
  1875  def test_subdomain_matching_with_ports():
```

### row #190 — `test_tag_order` @ `tests/test_json_tag.py:86`

```python
    83      assert isinstance(s.order[-2], Tag1)
    84  
    85      s.register(Tag2, index=None)
→   86      assert isinstance(s.order[-1], Tag2)
```

### row #191 — `test_template_filter` @ `tests/test_blueprints.py:429`

```python
   426          return s[::-1]
   427  
   428      app.register_blueprint(bp, url_prefix="/py")
→  429      assert "my_reverse" in app.jinja_env.filters.keys()
   430      assert app.jinja_env.filters["my_reverse"] == my_reverse
   431      assert app.jinja_env.filters["my_reverse"]("abcd") == "dcba"
   432  
```

### row #192 — `test_template_filter_with_template` @ `tests/test_templating.py:163`

```python
   160          return flask.render_template("template_filter.html", value="abcd")
   161  
   162      rv = client.get("/")
→  163      assert rv.data == b"dcba"
   164  
   165  
   166  def test_add_template_filter_with_template(app, client):
```

### row #193 — `test_template_loader_debugging` @ `tests/test_templating.py:409`

```python
   406              called.append(True)
   407              text = str(record.msg)
   408              assert "1: trying loader of application 'blueprintapp'" in text
→  409              assert (
   410                  "2: trying loader of blueprint 'admin' (blueprintapp.apps.admin)"
   411              ) in text
   412              assert (
```

### row #194 — `test_template_rendered` @ `tests/test_signals.py:30`

```python
    27          client.get("/")
    28          assert len(recorded) == 1
    29          template, context = recorded[0]
→   30          assert template.name == "simple_template.html"
    31          assert context["whiskey"] == 42
    32      finally:
    33          flask.template_rendered.disconnect(record, app)
```

### row #195 — `test_template_test` @ `tests/test_blueprints.py:567`

```python
   564      app.register_blueprint(bp, url_prefix="/py")
   565      assert "is_boolean" in app.jinja_env.tests.keys()
   566      assert app.jinja_env.tests["is_boolean"] == is_boolean
→  567      assert app.jinja_env.tests["is_boolean"](False)
   568  
   569  
   570  def test_add_template_test(app):
```

### row #196 — `test_templates_and_static` @ `tests/test_blueprints.py:222`

```python
   219          assert e.value.name == "missing.html"
   220  
   221      with flask.Flask(__name__).test_request_context():
→  222          assert flask.render_template("nested/nested.txt") == "I'm nested"
   223  
   224  
   225  def test_default_static_max_age(app):
```

### row #197 — `test_templates_auto_reload` @ `tests/test_templating.py:371`

```python
   368      app = flask.Flask(__name__)
   369      app.config["DEBUG"] = True
   370      assert app.config["TEMPLATES_AUTO_RELOAD"] is None
→  371      assert app.jinja_env.auto_reload is True
   372      # debug is True, config option is False
   373      app = flask.Flask(__name__)
   374      app.config["DEBUG"] = True
```

### row #198 — `test_url_generation` @ `tests/test_basic.py:1347`

```python
  1344      def hello():
  1345          pass
  1346  
→ 1347      assert flask.url_for("hello", name="test x") == "/hello/test%20x"
  1348      assert (
  1349          flask.url_for("hello", name="test x", _external=True)
  1350          == "http://localhost/hello/test%20x"
```

### row #199 — `test_url_mapping` @ `tests/test_basic.py:183`

```python
   180      assert sorted(rv.allow) == ["GET", "HEAD", "OPTIONS"]
   181      rv = client.head("/")
   182      assert rv.status_code == 200
→  183      assert not rv.data  # head truncates
   184      assert client.post("/more").data == b"POST"
   185      assert client.get("/more").data == b"GET"
   186      rv = client.delete("/more")
```

### row #200 — `test_view_decorators` @ `tests/test_views.py:97`

```python
    94  
    95      app.add_url_rule("/", view_func=Index.as_view("index"))
    96      rv = client.get("/")
→   97      assert rv.headers["X-Parachute"] == "awesome"
    98      assert rv.data == b"Awesome"
    99  
   100  
```
