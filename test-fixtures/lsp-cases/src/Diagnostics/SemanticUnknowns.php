<?php

declare(strict_types=1);

namespace App\Diagnostics;

use App\Model\User;
use App\Missing\Ghost;

// CASE: This unresolved class import should produce "Unresolved use statement".
use App\Missing\NotFoundDependency;

// CASE: Function/const imports are currently not validated by unresolved-use check.
use function App\Missing\missing_fn_alias;
use const App\Missing\MISSING_CONST;

// CASE: Known class usage (should NOT produce unknown-class diagnostic).
$known = new User('ok');

// ERROR: Alias resolves to App\Missing\Ghost -> should produce "Unknown class".
$ghost = new Ghost('x');

// ERROR: Fully-qualified missing class in new expression.
$missing = new \App\Missing\ExternalClass();

// ERROR: Unknown class in type hints (parameter + return type + property type).
function consume(\App\Model\User|Missing\TypeAlias $value): Missing\TypeAlias
{
    return $value;
}

// ERROR: Unknown base class and unknown interface in inheritance clauses.
class ChildController extends Missing\BaseController implements Missing\Contracts\Renderable
{
    // ERROR: Unknown class in property type.
    private Missing\Repo\Storage $storage;
}

// ERROR: Unknown namespaced function call should produce "Unknown function".
App\Utils\missing_helper();

// CASE: Built-in function should NOT be flagged as unknown function.
strlen('ok');

